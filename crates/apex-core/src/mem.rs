//! Guest physical memory.
//!
//! Guest RAM is a set of anonymous host mappings. On Apple Silicon the guest
//! and the host GPU share the same physical DRAM (UMA), so a pointer into
//! guest RAM can be wrapped by Metal (`newBufferWithBytesNoCopy`) without a
//! single copy; everything in this module is therefore built around stable,
//! page-aligned host addresses.

use std::fmt;
use std::sync::atomic::{fence, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::sys;

/// A guest physical (intermediate physical, IPA) address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GuestAddress(pub u64);

impl GuestAddress {
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
    #[inline]
    pub fn checked_add(self, off: u64) -> Option<GuestAddress> {
        self.0.checked_add(off).map(GuestAddress)
    }
    #[inline]
    pub fn unchecked_add(self, off: u64) -> GuestAddress {
        GuestAddress(self.0.wrapping_add(off))
    }
}

impl fmt::Debug for GuestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GPA({:#x})", self.0)
    }
}

impl fmt::Display for GuestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// Types that are valid for every bit pattern and have no padding
/// requirements beyond `repr(C)`; they can be copied to/from guest memory.
///
/// # Safety
/// Implementors must be `repr(C)`/primitive, contain no references, and accept
/// any byte pattern.
pub unsafe trait ByteValued: Copy + Default + Send + Sync + 'static {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: guaranteed by the trait contract.
        unsafe { std::slice::from_raw_parts(self as *const Self as *const u8, std::mem::size_of::<Self>()) }
    }
    fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: guaranteed by the trait contract.
        unsafe { std::slice::from_raw_parts_mut(self as *mut Self as *mut u8, std::mem::size_of::<Self>()) }
    }
    fn from_slice(b: &[u8]) -> Option<Self> {
        if b.len() < std::mem::size_of::<Self>() {
            return None;
        }
        let mut v = Self::default();
        v.as_mut_bytes().copy_from_slice(&b[..std::mem::size_of::<Self>()]);
        Some(v)
    }
}

macro_rules! byte_valued_prims {
    ($($t:ty),*) => { $( unsafe impl ByteValued for $t {} )* };
}
byte_valued_prims!(u8, u16, u32, u64, u128, i8, i16, i32, i64, usize, isize);
unsafe impl<const N: usize> ByteValued for [u8; N] where [u8; N]: Default {}

/// One contiguous guest-physical range backed by host memory.
#[derive(Clone, Copy)]
pub struct GuestRegion {
    base: GuestAddress,
    size: u64,
    host: *mut u8,
}

// SAFETY: the host pointer refers to memory that lives as long as the owning
// `GuestMemory` and is only ever accessed with volatile/atomic semantics.
unsafe impl Send for GuestRegion {}
unsafe impl Sync for GuestRegion {}

impl GuestRegion {
    pub fn base(&self) -> GuestAddress {
        self.base
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn end(&self) -> u64 {
        self.base.0 + self.size
    }
    pub fn host_ptr(&self) -> *mut u8 {
        self.host
    }
    #[inline]
    fn contains(&self, addr: u64, len: u64) -> bool {
        addr >= self.base.0 && len <= self.size && addr - self.base.0 <= self.size - len
    }
}

impl fmt::Debug for GuestRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:#x}..{:#x}) -> {:p}", self.base.0, self.end(), self.host)
    }
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

struct Inner {
    regions: Vec<GuestRegion>,
    mappings: Vec<Mapping>,
}

// SAFETY: see GuestRegion.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        for m in &self.mappings {
            // SAFETY: mappings were created by us and are unmapped exactly once.
            let _ = unsafe { sys::munmap_raw(m.ptr, m.len) };
        }
    }
}

/// Cheaply clonable handle to all guest RAM.
#[derive(Clone)]
pub struct GuestMemory {
    inner: Arc<Inner>,
}

impl fmt::Debug for GuestMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.inner.regions.iter()).finish()
    }
}

impl GuestMemory {
    /// Allocate host memory for each `(base, size)` range. Ranges must be
    /// host-page aligned and must not overlap.
    pub fn new(ranges: &[(GuestAddress, u64)]) -> Result<Self> {
        let page = sys::host_page_size() as u64;
        let mut sorted = ranges.to_vec();
        sorted.sort_by_key(|r| r.0);
        for w in sorted.windows(2) {
            if w[0].0 .0 + w[0].1 > w[1].0 .0 {
                return Err(Error::Memory(format!("overlapping regions {:?} and {:?}", w[0], w[1])));
            }
        }
        let mut regions = Vec::new();
        let mut mappings = Vec::new();
        for (base, size) in sorted {
            if size == 0 || base.0 % page != 0 || size % page != 0 {
                return Err(Error::Memory(format!("region {base} size {size:#x} not aligned to host page {page:#x}")));
            }
            let host = sys::mmap_anonymous(size as usize).map_err(|e| Error::Memory(format!("mmap {size:#x} bytes: {e}")))?;
            mappings.push(Mapping { ptr: host, len: size as usize });
            regions.push(GuestRegion { base, size, host });
        }
        Ok(GuestMemory { inner: Arc::new(Inner { regions, mappings }) })
    }

    pub fn regions(&self) -> &[GuestRegion] {
        &self.inner.regions
    }

    pub fn total_size(&self) -> u64 {
        self.inner.regions.iter().map(|r| r.size).sum()
    }

    /// Highest guest-physical address + 1 covered by RAM.
    pub fn end_addr(&self) -> GuestAddress {
        GuestAddress(self.inner.regions.iter().map(|r| r.end()).max().unwrap_or(0))
    }

    #[inline]
    fn region_for(&self, addr: u64, len: u64) -> Option<&GuestRegion> {
        // Region count is tiny (1-3), a linear scan beats a tree.
        self.inner.regions.iter().find(|r| r.contains(addr, len))
    }

    pub fn is_valid_range(&self, addr: GuestAddress, len: u64) -> bool {
        self.region_for(addr.0, len).is_some()
    }

    /// Host pointer for `[addr, addr+len)`, which must lie in one region.
    #[inline]
    pub fn host_ptr(&self, addr: GuestAddress, len: u64) -> Result<*mut u8> {
        let r = self.region_for(addr.0, len).ok_or_else(|| Error::Memory(format!("access {addr}+{len:#x} outside guest RAM")))?;
        // SAFETY: bounds checked above.
        Ok(unsafe { r.host.add((addr.0 - r.base.0) as usize) })
    }

    pub fn read(&self, buf: &mut [u8], addr: GuestAddress) -> Result<()> {
        let p = self.host_ptr(addr, buf.len() as u64)?;
        // SAFETY: range validated; guest memory is never unmapped while `self` lives.
        unsafe { std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    pub fn write(&self, buf: &[u8], addr: GuestAddress) -> Result<()> {
        let p = self.host_ptr(addr, buf.len() as u64)?;
        // SAFETY: range validated.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), p, buf.len()) };
        Ok(())
    }

    pub fn fill(&self, addr: GuestAddress, len: u64, byte: u8) -> Result<()> {
        let p = self.host_ptr(addr, len)?;
        // SAFETY: range validated.
        unsafe { std::ptr::write_bytes(p, byte, len as usize) };
        Ok(())
    }

    pub fn read_obj<T: ByteValued>(&self, addr: GuestAddress) -> Result<T> {
        let p = self.host_ptr(addr, std::mem::size_of::<T>() as u64)?;
        if (p as usize) % std::mem::align_of::<T>() == 0 {
            // SAFETY: range validated and aligned; T accepts any bit pattern.
            Ok(unsafe { std::ptr::read_volatile(p as *const T) })
        } else {
            let mut out = T::default();
            // SAFETY: range validated above.
            unsafe { std::ptr::copy_nonoverlapping(p, out.as_mut_bytes().as_mut_ptr(), std::mem::size_of::<T>()) };
            Ok(out)
        }
    }

    pub fn write_obj<T: ByteValued>(&self, val: T, addr: GuestAddress) -> Result<()> {
        let p = self.host_ptr(addr, std::mem::size_of::<T>() as u64)?;
        if (p as usize) % std::mem::align_of::<T>() == 0 {
            // SAFETY: range validated and aligned.
            unsafe { std::ptr::write_volatile(p as *mut T, val) };
        } else {
            // SAFETY: range validated.
            unsafe { std::ptr::copy_nonoverlapping(val.as_bytes().as_ptr(), p, std::mem::size_of::<T>()) };
        }
        Ok(())
    }

    /// Atomic acquire load of a naturally aligned little-endian u16 (virtqueue
    /// ring indices).
    pub fn load_u16_acquire(&self, addr: GuestAddress) -> Result<u16> {
        let p = self.host_ptr(addr, 2)?;
        if (p as usize) & 1 != 0 {
            return Err(Error::Memory(format!("unaligned atomic u16 at {addr}")));
        }
        // SAFETY: aligned, in-bounds, lives as long as self.
        let a = unsafe { &*(p as *const AtomicU16) };
        Ok(u16::from_le(a.load(Ordering::Acquire)))
    }

    pub fn store_u16_release(&self, val: u16, addr: GuestAddress) -> Result<()> {
        let p = self.host_ptr(addr, 2)?;
        if (p as usize) & 1 != 0 {
            return Err(Error::Memory(format!("unaligned atomic u16 at {addr}")));
        }
        // SAFETY: aligned, in-bounds.
        let a = unsafe { &*(p as *const AtomicU16) };
        a.store(val.to_le(), Ordering::Release);
        Ok(())
    }

    pub fn load_u32_acquire(&self, addr: GuestAddress) -> Result<u32> {
        let p = self.host_ptr(addr, 4)?;
        if (p as usize) & 3 != 0 {
            return Err(Error::Memory(format!("unaligned atomic u32 at {addr}")));
        }
        // SAFETY: aligned, in-bounds.
        let a = unsafe { &*(p as *const AtomicU32) };
        Ok(u32::from_le(a.load(Ordering::Acquire)))
    }

    /// Full barrier used around virtqueue publish/consume points.
    #[inline]
    pub fn barrier() {
        fence(Ordering::SeqCst);
    }

    /// Return guest pages to the host (used by free page reporting).
    pub fn discard(&self, addr: GuestAddress, len: u64) -> Result<()> {
        let page = sys::host_page_size() as u64;
        if addr.0 % page != 0 || len % page != 0 {
            return Err(Error::Memory("discard range not page aligned".into()));
        }
        let p = self.host_ptr(addr, len)?;
        // SAFETY: range is inside one of our anonymous mappings.
        unsafe { sys::discard_pages(p, len as usize) }.map_err(Error::Io)
    }
}

/// Little-endian helpers for device register files.
#[inline]
pub fn le_read(data: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    let n = data.len().min(8);
    v[..n].copy_from_slice(&data[..n]);
    u64::from_le_bytes(v)
}

#[inline]
pub fn le_write(data: &mut [u8], v: u64) {
    let b = v.to_le_bytes();
    let n = data.len().min(8);
    data[..n].copy_from_slice(&b[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> u64 {
        sys::host_page_size() as u64
    }

    #[test]
    fn read_write_roundtrip() {
        let p = page();
        let m = GuestMemory::new(&[(GuestAddress(0x8000_0000), 16 * p), (GuestAddress(0x1_0000_0000), 4 * p)]).unwrap();
        assert_eq!(m.total_size(), 20 * p);
        m.write_obj(0xdead_beefu32, GuestAddress(0x8000_0010)).unwrap();
        assert_eq!(m.read_obj::<u32>(GuestAddress(0x8000_0010)).unwrap(), 0xdead_beef);
        // unaligned object access
        m.write_obj(0x1122_3344_5566_7788u64, GuestAddress(0x8000_0003)).unwrap();
        assert_eq!(m.read_obj::<u64>(GuestAddress(0x8000_0003)).unwrap(), 0x1122_3344_5566_7788);
        let mut buf = [0u8; 4];
        m.write(&[1, 2, 3, 4], GuestAddress(0x1_0000_0000)).unwrap();
        m.read(&mut buf, GuestAddress(0x1_0000_0000)).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        m.store_u16_release(0x55aa, GuestAddress(0x8000_0100)).unwrap();
        assert_eq!(m.load_u16_acquire(GuestAddress(0x8000_0100)).unwrap(), 0x55aa);
        assert_eq!(m.end_addr(), GuestAddress(0x1_0000_0000 + 4 * p));
    }

    #[test]
    fn bounds_are_enforced() {
        let p = page();
        let m = GuestMemory::new(&[(GuestAddress(0x4000_0000), 2 * p)]).unwrap();
        assert!(m.read_obj::<u32>(GuestAddress(0x4000_0000 + 2 * p - 2)).is_err());
        assert!(m.read_obj::<u8>(GuestAddress(0x3fff_ffff)).is_err());
        assert!(m.host_ptr(GuestAddress(0x4000_0000), 2 * p).is_ok());
        assert!(m.host_ptr(GuestAddress(0x4000_0000), 2 * p + 1).is_err());
        assert!(m.host_ptr(GuestAddress(u64::MAX - 1), 4).is_err());
        assert!(m.load_u16_acquire(GuestAddress(0x4000_0001)).is_err());
    }

    #[test]
    fn rejects_bad_layout() {
        let p = page();
        assert!(GuestMemory::new(&[(GuestAddress(0), p), (GuestAddress(0), p)]).is_err());
        assert!(GuestMemory::new(&[(GuestAddress(1), p)]).is_err());
    }
}
