//! MMIO bus.
//!
//! The bus is built once while the machine is assembled and is immutable
//! afterwards, so a vCPU trap is dispatched with a lock-free binary search.
//! Devices do their own fine-grained locking.

use std::fmt;
use std::sync::Arc;

use crate::error::{Error, Result};

/// A memory-mapped device. `offset` is relative to the device's base.
/// `data.len()` is 1, 2, 4 or 8.
pub trait MmioDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
    fn name(&self) -> &str {
        "mmio-device"
    }
}

#[derive(Clone)]
struct Entry {
    base: u64,
    size: u64,
    dev: Arc<dyn MmioDevice>,
}

#[derive(Default, Clone)]
pub struct MmioBus {
    entries: Vec<Entry>,
}

impl fmt::Debug for MmioBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut l = f.debug_list();
        for e in &self.entries {
            l.entry(&format_args!("{:#010x}+{:#x} {}", e.base, e.size, e.dev.name()));
        }
        l.finish()
    }
}

impl MmioBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, base: u64, size: u64, dev: Arc<dyn MmioDevice>) -> Result<()> {
        if size == 0 || base.checked_add(size).is_none() {
            return Err(Error::Device(format!("bad MMIO range {base:#x}+{size:#x}")));
        }
        let idx = self.entries.partition_point(|e| e.base < base);
        if let Some(prev) = idx.checked_sub(1).and_then(|i| self.entries.get(i)) {
            if prev.base + prev.size > base {
                return Err(Error::Device(format!("MMIO {} at {base:#x} overlaps {} at {:#x}", dev.name(), prev.dev.name(), prev.base)));
            }
        }
        if let Some(next) = self.entries.get(idx) {
            if base + size > next.base {
                return Err(Error::Device(format!("MMIO {} at {base:#x} overlaps {} at {:#x}", dev.name(), next.dev.name(), next.base)));
            }
        }
        self.entries.insert(idx, Entry { base, size, dev });
        Ok(())
    }

    #[inline]
    fn lookup(&self, addr: u64, len: usize) -> Option<(&Entry, u64)> {
        let idx = self.entries.partition_point(|e| e.base <= addr);
        let e = self.entries.get(idx.checked_sub(1)?)?;
        let off = addr - e.base;
        if off + len as u64 <= e.size {
            Some((e, off))
        } else {
            None
        }
    }

    /// Returns false if no device claims the address (the caller decides
    /// whether that is RAZ/WI or an external abort).
    #[inline]
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.lookup(addr, data.len()) {
            Some((e, off)) => {
                e.dev.read(off, data);
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.lookup(addr, data.len()) {
            Some((e, off)) => {
                e.dev.write(off, data);
                true
            }
            None => false,
        }
    }

    pub fn device_at(&self, addr: u64) -> Option<Arc<dyn MmioDevice>> {
        self.lookup(addr, 1).map(|(e, _)| e.dev.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Reg(Mutex<u32>, &'static str);
    impl MmioDevice for Reg {
        fn read(&self, off: u64, data: &mut [u8]) {
            let v = *self.0.lock().unwrap() + off as u32;
            data.copy_from_slice(&v.to_le_bytes()[..data.len()]);
        }
        fn write(&self, _off: u64, data: &[u8]) {
            let mut b = [0u8; 4];
            b[..data.len()].copy_from_slice(data);
            *self.0.lock().unwrap() = u32::from_le_bytes(b);
        }
        fn name(&self) -> &str {
            self.1
        }
    }

    #[test]
    fn dispatch_and_overlap() {
        let mut bus = MmioBus::new();
        bus.insert(0x1000, 0x100, Arc::new(Reg(Mutex::new(0), "a"))).unwrap();
        bus.insert(0x3000, 0x100, Arc::new(Reg(Mutex::new(100), "b"))).unwrap();
        bus.insert(0x2000, 0x1000, Arc::new(Reg(Mutex::new(200), "c"))).unwrap();
        assert!(bus.insert(0x10ff, 2, Arc::new(Reg(Mutex::new(0), "x"))).is_err());
        assert!(bus.insert(0x0f00, 0x101, Arc::new(Reg(Mutex::new(0), "y"))).is_err());
        let mut d = [0u8; 4];
        assert!(bus.read(0x3004, &mut d));
        assert_eq!(u32::from_le_bytes(d), 104);
        assert!(bus.write(0x1000, &7u32.to_le_bytes()));
        assert!(bus.read(0x1000, &mut d));
        assert_eq!(u32::from_le_bytes(d), 7);
        assert!(!bus.read(0x10fe, &mut d)); // straddles the end
        assert!(!bus.read(0x5000, &mut d));
        assert!(!bus.read(0x0, &mut d));
        assert_eq!(bus.device_at(0x2fff).unwrap().name(), "c");
    }
}
