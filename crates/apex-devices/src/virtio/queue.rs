//! Split virtqueue (virtio 1.2, 2.7) with EVENT_IDX notification suppression
//! and indirect descriptors.

use std::io;

use apex_core::mem::{ByteValued, GuestAddress, GuestMemory};
use apex_core::{Error, Result};

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const DESC_F_INDIRECT: u16 = 4;
const AVAIL_F_NO_INTERRUPT: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: GuestAddress,
    pub len: u32,
    pub flags: u16,
}

impl Descriptor {
    pub fn is_write_only(&self) -> bool {
        self.flags & DESC_F_WRITE != 0
    }
}

/// A resolved descriptor chain (indirect tables are flattened).
#[derive(Clone, Debug)]
pub struct DescriptorChain {
    pub head: u16,
    pub descs: Vec<Descriptor>,
}

impl DescriptorChain {
    pub fn reader<'a>(&self, mem: &'a GuestMemory) -> Reader<'a> {
        Reader::new(mem, self.descs.iter().filter(|d| !d.is_write_only()).copied().collect())
    }

    pub fn writer<'a>(&self, mem: &'a GuestMemory) -> Writer<'a> {
        Writer::new(mem, self.descs.iter().filter(|d| d.is_write_only()).copied().collect())
    }

    pub fn readable_len(&self) -> usize {
        self.descs.iter().filter(|d| !d.is_write_only()).map(|d| d.len as usize).sum()
    }

    pub fn writable_len(&self) -> usize {
        self.descs.iter().filter(|d| d.is_write_only()).map(|d| d.len as usize).sum()
    }
}

#[derive(Clone, Debug)]
pub struct Queue {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc_table: GuestAddress,
    pub avail_ring: GuestAddress,
    pub used_ring: GuestAddress,
    next_avail: u16,
    next_used: u16,
    last_signalled_used: u16,
    event_idx: bool,
}

impl Queue {
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: max_size,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: 0,
            next_used: 0,
            last_signalled_used: 0,
            event_idx: false,
        }
    }

    pub fn set_event_idx(&mut self, on: bool) {
        self.event_idx = on;
    }

    pub fn next_avail(&self) -> u16 {
        self.next_avail
    }

    pub fn next_used(&self) -> u16 {
        self.next_used
    }

    /// Validate the queue layout the driver programmed.
    pub fn validate(&self, mem: &GuestMemory) -> Result<()> {
        let n = self.size as u64;
        if self.size == 0 || !self.size.is_power_of_two() || self.size > self.max_size {
            return Err(Error::Device(format!("invalid queue size {}", self.size)));
        }
        if self.desc_table.0 % 16 != 0 || self.avail_ring.0 % 2 != 0 || self.used_ring.0 % 4 != 0 {
            return Err(Error::Device("misaligned virtqueue".into()));
        }
        let ok = mem.is_valid_range(self.desc_table, 16 * n)
            && mem.is_valid_range(self.avail_ring, 6 + 2 * n)
            && mem.is_valid_range(self.used_ring, 6 + 8 * n);
        if !ok {
            return Err(Error::Device("virtqueue rings outside guest RAM".into()));
        }
        Ok(())
    }

    #[inline]
    fn avail_idx(&self, mem: &GuestMemory) -> Result<u16> {
        mem.load_u16_acquire(self.avail_ring.unchecked_add(2))
    }

    /// Number of chains the driver has made available that we have not popped.
    pub fn pending(&self, mem: &GuestMemory) -> u16 {
        self.avail_idx(mem).map(|i| i.wrapping_sub(self.next_avail)).unwrap_or(0)
    }

    fn read_desc(mem: &GuestMemory, table: GuestAddress, idx: u16) -> Result<(Descriptor, u16)> {
        let base = table.unchecked_add(idx as u64 * 16);
        let addr: u64 = mem.read_obj(base)?;
        let len: u32 = mem.read_obj(base.unchecked_add(8))?;
        let flags: u16 = mem.read_obj(base.unchecked_add(12))?;
        let next: u16 = mem.read_obj(base.unchecked_add(14))?;
        Ok((Descriptor { addr: GuestAddress(u64::from_le(addr)), len: u32::from_le(len), flags: u16::from_le(flags) }, u16::from_le(next)))
    }

    fn walk(&self, mem: &GuestMemory, head: u16) -> Result<Vec<Descriptor>> {
        let mut out = Vec::new();
        let mut table = self.desc_table;
        let mut table_len = self.size;
        let mut idx = head;
        let mut indirect = false;
        let mut budget = self.size as usize;
        loop {
            if idx >= table_len {
                return Err(Error::Device(format!("descriptor index {idx} out of range")));
            }
            let (d, next) = Self::read_desc(mem, table, idx)?;
            if d.flags & DESC_F_INDIRECT != 0 {
                if indirect || d.flags & DESC_F_NEXT != 0 {
                    return Err(Error::Device("nested or chained indirect descriptor".into()));
                }
                if d.len == 0 || d.len % 16 != 0 || d.len / 16 > u16::MAX as u32 + 1 {
                    return Err(Error::Device("bad indirect table length".into()));
                }
                if !mem.is_valid_range(d.addr, d.len as u64) {
                    return Err(Error::Device("indirect table outside guest RAM".into()));
                }
                indirect = true;
                table = d.addr;
                table_len = (d.len / 16) as u16;
                budget = table_len as usize;
                idx = 0;
                continue;
            }
            if d.len > 0 && !mem.is_valid_range(d.addr, d.len as u64) {
                return Err(Error::Device(format!("descriptor {:?}+{:#x} outside guest RAM", d.addr, d.len)));
            }
            out.push(d);
            if d.flags & DESC_F_NEXT == 0 {
                return Ok(out);
            }
            budget = budget.checked_sub(1).ok_or_else(|| Error::Device("descriptor loop".into()))?;
            if budget == 0 {
                return Err(Error::Device("descriptor chain too long".into()));
            }
            idx = next;
        }
    }

    /// Pop the next available chain. Malformed chains are returned to the
    /// driver with length 0 and skipped.
    pub fn pop(&mut self, mem: &GuestMemory) -> Option<DescriptorChain> {
        loop {
            let avail = self.avail_idx(mem).ok()?;
            if avail == self.next_avail {
                return None;
            }
            let slot = self.next_avail % self.size;
            let head: u16 = mem.read_obj(self.avail_ring.unchecked_add(4 + 2 * slot as u64)).ok().map(u16::from_le)?;
            self.next_avail = self.next_avail.wrapping_add(1);
            if self.event_idx {
                self.set_avail_event(mem);
            }
            match self.walk(mem, head) {
                Ok(descs) => return Some(DescriptorChain { head, descs }),
                Err(e) => {
                    apex_core::warn!("dropping malformed chain {head}: {e}");
                    let _ = self.add_used(mem, head, 0);
                }
            }
        }
    }

    /// Put a popped chain back (device could not process it yet).
    pub fn undo_pop(&mut self) {
        self.next_avail = self.next_avail.wrapping_sub(1);
    }

    fn set_avail_event(&self, mem: &GuestMemory) {
        let at = self.used_ring.unchecked_add(4 + 8 * self.size as u64);
        let _ = mem.write_obj(self.next_avail.to_le(), at);
    }

    /// Re-arm driver notifications before a worker goes to sleep. Returns
    /// true if more buffers arrived meanwhile (the caller must not sleep).
    pub fn enable_notification(&mut self, mem: &GuestMemory) -> bool {
        if self.event_idx {
            self.set_avail_event(mem);
        }
        GuestMemory::barrier();
        self.pending(mem) != 0
    }

    pub fn add_used(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<()> {
        let slot = self.next_used % self.size;
        let elem = self.used_ring.unchecked_add(4 + 8 * slot as u64);
        mem.write_obj((head as u32).to_le(), elem)?;
        mem.write_obj(len.to_le(), elem.unchecked_add(4))?;
        self.next_used = self.next_used.wrapping_add(1);
        mem.store_u16_release(self.next_used, self.used_ring.unchecked_add(2))
    }

    /// Whether the driver wants an interrupt for the used entries added
    /// since the last call (EVENT_IDX aware).
    pub fn needs_notification(&mut self, mem: &GuestMemory) -> bool {
        GuestMemory::barrier();
        let new = self.next_used;
        let old = self.last_signalled_used;
        if new == old {
            return false;
        }
        self.last_signalled_used = new;
        if self.event_idx {
            let used_event: u16 = match mem.read_obj(self.avail_ring.unchecked_add(4 + 2 * self.size as u64)) {
                Ok(v) => u16::from_le(v),
                Err(_) => return true,
            };
            new.wrapping_sub(used_event).wrapping_sub(1) < new.wrapping_sub(old)
        } else {
            let flags: u16 = mem.read_obj(self.avail_ring).map(u16::from_le).unwrap_or(0);
            flags & AVAIL_F_NO_INTERRUPT == 0
        }
    }

    pub fn reset(&mut self) {
        *self = Queue::new(self.max_size);
    }
}

/// Sequential reader over the device-readable part of a chain.
pub struct Reader<'a> {
    mem: &'a GuestMemory,
    descs: Vec<Descriptor>,
    idx: usize,
    off: usize,
    consumed: usize,
}

impl<'a> Reader<'a> {
    fn new(mem: &'a GuestMemory, descs: Vec<Descriptor>) -> Self {
        Reader { mem, descs, idx: 0, off: 0, consumed: 0 }
    }

    pub fn available(&self) -> usize {
        self.descs.iter().skip(self.idx).map(|d| d.len as usize).sum::<usize>() - self.off
    }

    pub fn bytes_read(&self) -> usize {
        self.consumed
    }

    /// Visit the next contiguous guest memory segments, up to `max` bytes.
    /// `f` returns how many bytes it consumed from the slice.
    pub fn consume_with<F>(&mut self, max: usize, mut f: F) -> io::Result<usize>
    where
        F: FnMut(&[u8]) -> io::Result<usize>,
    {
        let mut total = 0;
        while total < max && self.idx < self.descs.len() {
            let d = self.descs[self.idx];
            let left = d.len as usize - self.off;
            if left == 0 {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let n = left.min(max - total);
            let p = self
                .mem
                .host_ptr(d.addr.unchecked_add(self.off as u64), n as u64)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            // SAFETY: range validated when the chain was built; guest memory
            // outlives the reader. The guest may modify it concurrently, which
            // is the accepted semantics of virtio buffers.
            let s = unsafe { std::slice::from_raw_parts(p, n) };
            let used = f(s)?;
            total += used;
            self.consumed += used;
            self.off += used;
            if used < n {
                break;
            }
        }
        Ok(total)
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut pos = 0;
        let _ = self.consume_with(buf.len(), |s| {
            buf[pos..pos + s.len()].copy_from_slice(s);
            pos += s.len();
            Ok(s.len())
        });
        pos
    }

    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        if self.read(buf) != buf.len() {
            return Err(Error::Device("descriptor chain too short".into()));
        }
        Ok(())
    }

    pub fn read_obj<T: ByteValued>(&mut self) -> Result<T> {
        let mut v = T::default();
        self.read_exact(v.as_mut_bytes())?;
        Ok(v)
    }

    pub fn read_to_vec(&mut self, max: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.available().min(max));
        let _ = self.consume_with(max, |s| {
            v.extend_from_slice(s);
            Ok(s.len())
        });
        v
    }

    pub fn skip(&mut self, n: usize) -> usize {
        self.consume_with(n, |s| Ok(s.len())).unwrap_or(0)
    }

    /// Remaining readable segments as (host pointer, len) pairs.
    pub fn remaining_iovecs(&self) -> Vec<(*mut u8, usize)> {
        let mut v = Vec::new();
        for (i, d) in self.descs.iter().enumerate().skip(self.idx) {
            let off = if i == self.idx { self.off } else { 0 };
            let len = d.len as usize - off;
            if len == 0 {
                continue;
            }
            if let Ok(p) = self.mem.host_ptr(d.addr.unchecked_add(off as u64), len as u64) {
                v.push((p, len));
            }
        }
        v
    }
}

/// Sequential writer over the device-writable part of a chain.
pub struct Writer<'a> {
    mem: &'a GuestMemory,
    descs: Vec<Descriptor>,
    idx: usize,
    off: usize,
    written: usize,
}

impl<'a> Writer<'a> {
    fn new(mem: &'a GuestMemory, descs: Vec<Descriptor>) -> Self {
        Writer { mem, descs, idx: 0, off: 0, written: 0 }
    }

    pub fn available(&self) -> usize {
        self.descs.iter().skip(self.idx).map(|d| d.len as usize).sum::<usize>() - self.off
    }

    pub fn bytes_written(&self) -> usize {
        self.written
    }

    /// Hand out writable guest memory segments to `f` (e.g. `pread` directly
    /// into guest RAM, zero copy). `f` returns bytes produced.
    pub fn produce_with<F>(&mut self, max: usize, mut f: F) -> io::Result<usize>
    where
        F: FnMut(&mut [u8]) -> io::Result<usize>,
    {
        let mut total = 0;
        while total < max && self.idx < self.descs.len() {
            let d = self.descs[self.idx];
            let left = d.len as usize - self.off;
            if left == 0 {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let n = left.min(max - total);
            let p = self
                .mem
                .host_ptr(d.addr.unchecked_add(self.off as u64), n as u64)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            // SAFETY: see Reader::consume_with.
            let s = unsafe { std::slice::from_raw_parts_mut(p, n) };
            let used = f(s)?;
            total += used;
            self.written += used;
            self.off += used;
            if used < n {
                break;
            }
        }
        Ok(total)
    }

    pub fn write(&mut self, buf: &[u8]) -> usize {
        let mut pos = 0;
        let _ = self.produce_with(buf.len(), |s| {
            s.copy_from_slice(&buf[pos..pos + s.len()]);
            pos += s.len();
            Ok(s.len())
        });
        pos
    }

    pub fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        if self.write(buf) != buf.len() {
            return Err(Error::Device("descriptor chain too small".into()));
        }
        Ok(())
    }

    pub fn write_obj<T: ByteValued>(&mut self, v: T) -> Result<()> {
        self.write_all(v.as_bytes())
    }

    /// Split off a writer covering only the last `n` bytes (e.g. the virtio-blk
    /// status byte) and truncate `self` before them.
    pub fn split_tail(&mut self, n: usize) -> Option<Writer<'a>> {
        let total = self.available();
        if total < n {
            return None;
        }
        let mut keep = total - n;
        let mut head = Vec::new();
        let mut tail = Vec::new();
        for (i, d) in self.descs.iter().enumerate().skip(self.idx) {
            let off = if i == self.idx { self.off } else { 0 };
            let len = d.len as usize - off;
            let base = d.addr.unchecked_add(off as u64);
            if keep >= len {
                head.push(Descriptor { addr: base, len: len as u32, flags: d.flags });
                keep -= len;
            } else {
                if keep > 0 {
                    head.push(Descriptor { addr: base, len: keep as u32, flags: d.flags });
                }
                tail.push(Descriptor { addr: base.unchecked_add(keep as u64), len: (len - keep) as u32, flags: d.flags });
                keep = 0;
            }
        }
        self.descs = head;
        self.idx = 0;
        self.off = 0;
        Some(Writer::new(self.mem, tail))
    }
}

/// Helpers to build virtqueues in guest memory for unit tests (acts as the
/// guest driver).
#[cfg(test)]
pub(crate) mod test_driver {
    use super::*;
    use apex_core::sys;

    pub struct Driver {
        pub mem: GuestMemory,
        pub q: Queue,
        pub size: u16,
        next_desc: u16,
        avail_idx: u16,
        pub data_cursor: u64,
        used_seen: u16,
    }

    pub const RAM: u64 = 0x4000_0000;

    impl Driver {
        pub fn new(size: u16) -> Driver {
            let page = sys::host_page_size() as u64;
            let len = apex_core::align_up(4 << 20, page);
            let mem = GuestMemory::new(&[(GuestAddress(RAM), len)]).unwrap();
            let mut q = Queue::new(size);
            q.size = size;
            q.desc_table = GuestAddress(RAM);
            q.avail_ring = GuestAddress(RAM + 0x1_0000);
            q.used_ring = GuestAddress(RAM + 0x2_0000);
            q.ready = true;
            q.validate(&mem).unwrap();
            Driver { mem, q, size, next_desc: 0, avail_idx: 0, data_cursor: RAM + 0x10_0000, used_seen: 0 }
        }

        /// A second queue in the same guest memory (e.g. a TX queue).
        pub fn sibling(&self) -> Driver {
            let mut q = Queue::new(self.size);
            q.size = self.size;
            q.desc_table = GuestAddress(RAM + 0x3_0000);
            q.avail_ring = GuestAddress(RAM + 0x4_0000);
            q.used_ring = GuestAddress(RAM + 0x5_0000);
            q.ready = true;
            Driver { mem: self.mem.clone(), q, size: self.size, next_desc: 0, avail_idx: 0, data_cursor: RAM + 0x28_0000, used_seen: 0 }
        }

        pub fn alloc(&mut self, len: usize) -> GuestAddress {
            let a = GuestAddress(self.data_cursor);
            self.data_cursor += apex_core::align_up(len.max(1) as u64, 64);
            a
        }

        fn write_desc(&mut self, i: u16, addr: GuestAddress, len: u32, flags: u16, next: u16) {
            let base = self.q.desc_table.unchecked_add(i as u64 * 16);
            self.mem.write_obj(addr.0, base).unwrap();
            self.mem.write_obj(len, base.unchecked_add(8)).unwrap();
            self.mem.write_obj(flags, base.unchecked_add(12)).unwrap();
            self.mem.write_obj(next, base.unchecked_add(14)).unwrap();
        }

        /// Add a chain of (data, writable_len) segments: readable segments carry
        /// bytes, writable ones reserve space. Returns head and the writable
        /// buffer addresses.
        pub fn add_chain(&mut self, readable: &[&[u8]], writable: &[usize]) -> (u16, Vec<GuestAddress>) {
            let head = self.next_desc;
            let total = readable.len() + writable.len();
            let mut outs = Vec::new();
            for (k, r) in readable.iter().enumerate() {
                let a = self.alloc(r.len());
                self.mem.write(r, a).unwrap();
                let i = self.next_desc;
                let flags = if k + 1 < total { DESC_F_NEXT } else { 0 };
                self.write_desc(i, a, r.len() as u32, flags, (i + 1) % self.size);
                self.next_desc = (self.next_desc + 1) % self.size;
            }
            for (k, &w) in writable.iter().enumerate() {
                let a = self.alloc(w);
                outs.push(a);
                let i = self.next_desc;
                let flags = DESC_F_WRITE | if readable.len() + k + 1 < total { DESC_F_NEXT } else { 0 };
                self.write_desc(i, a, w as u32, flags, (i + 1) % self.size);
                self.next_desc = (self.next_desc + 1) % self.size;
            }
            self.publish(head);
            (head, outs)
        }

        /// Add a chain through an indirect table.
        pub fn add_indirect(&mut self, readable: &[&[u8]], writable: &[usize]) -> (u16, Vec<GuestAddress>) {
            let n = readable.len() + writable.len();
            let table = self.alloc(n * 16);
            let mut outs = Vec::new();
            let mut i = 0u16;
            let put = |drv: &mut Driver, addr: GuestAddress, len: u32, w: bool, i: u16| {
                let base = table.unchecked_add(i as u64 * 16);
                let flags = if w { DESC_F_WRITE } else { 0 } | if (i as usize) + 1 < n { DESC_F_NEXT } else { 0 };
                drv.mem.write_obj(addr.0, base).unwrap();
                drv.mem.write_obj(len, base.unchecked_add(8)).unwrap();
                drv.mem.write_obj(flags, base.unchecked_add(12)).unwrap();
                drv.mem.write_obj(i + 1, base.unchecked_add(14)).unwrap();
            };
            for r in readable {
                let a = self.alloc(r.len());
                self.mem.write(r, a).unwrap();
                put(self, a, r.len() as u32, false, i);
                i += 1;
            }
            for &w in writable {
                let a = self.alloc(w);
                outs.push(a);
                put(self, a, w as u32, true, i);
                i += 1;
            }
            let head = self.next_desc;
            self.write_desc(head, table, (n * 16) as u32, DESC_F_INDIRECT, 0);
            self.next_desc = (self.next_desc + 1) % self.size;
            self.publish(head);
            (head, outs)
        }

        fn publish(&mut self, head: u16) {
            let slot = self.avail_idx % self.size;
            self.mem.write_obj(head, self.q.avail_ring.unchecked_add(4 + 2 * slot as u64)).unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.mem.store_u16_release(self.avail_idx, self.q.avail_ring.unchecked_add(2)).unwrap();
        }

        pub fn set_used_event(&mut self, v: u16) {
            self.mem.write_obj(v, self.q.avail_ring.unchecked_add(4 + 2 * self.size as u64)).unwrap();
        }

        /// Collect newly used (head, len) entries.
        pub fn take_used(&mut self) -> Vec<(u16, u32)> {
            let idx = self.mem.load_u16_acquire(self.q.used_ring.unchecked_add(2)).unwrap();
            let mut v = Vec::new();
            while self.used_seen != idx {
                let slot = self.used_seen % self.size;
                let e = self.q.used_ring.unchecked_add(4 + 8 * slot as u64);
                let id: u32 = self.mem.read_obj(e).unwrap();
                let len: u32 = self.mem.read_obj(e.unchecked_add(4)).unwrap();
                v.push((id as u16, len));
                self.used_seen = self.used_seen.wrapping_add(1);
            }
            v
        }

        pub fn read(&self, a: GuestAddress, len: usize) -> Vec<u8> {
            let mut v = vec![0u8; len];
            self.mem.read(&mut v, a).unwrap();
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_driver::*;
    use super::*;

    #[test]
    fn pop_read_write_used() {
        let mut d = Driver::new(8);
        let (head, outs) = d.add_chain(&[b"hello ", b"world"], &[4, 8]);
        let mut q = d.q.clone();
        let chain = q.pop(&d.mem).unwrap();
        assert_eq!(chain.head, head);
        assert_eq!(chain.readable_len(), 11);
        assert_eq!(chain.writable_len(), 12);
        let mut r = chain.reader(&d.mem);
        assert_eq!(r.read_to_vec(100), b"hello world");
        let mut w = chain.writer(&d.mem);
        let mut tail = w.split_tail(1).unwrap();
        w.write_all(b"0123456789A").unwrap();
        assert!(w.write_all(b"x").is_err());
        tail.write_all(&[0x5a]).unwrap();
        q.add_used(&d.mem, chain.head, 12).unwrap();
        assert!(q.pop(&d.mem).is_none());
        assert_eq!(d.take_used(), vec![(head, 12)]);
        assert_eq!(d.read(outs[0], 4), b"0123");
        assert_eq!(d.read(outs[1], 8), b"456789AZ");
    }

    #[test]
    fn indirect_chains() {
        let mut d = Driver::new(4);
        let (head, outs) = d.add_indirect(&[b"abc"], &[3]);
        let mut q = d.q.clone();
        let c = q.pop(&d.mem).unwrap();
        assert_eq!(c.head, head);
        assert_eq!(c.descs.len(), 2);
        c.writer(&d.mem).write_all(b"xyz").unwrap();
        assert_eq!(d.read(outs[0], 3), b"xyz");
    }

    #[test]
    fn event_idx_suppression() {
        let mut d = Driver::new(8);
        let mut q = d.q.clone();
        q.set_event_idx(true);
        for _ in 0..3 {
            d.add_chain(&[b"x"], &[]);
        }
        // Driver asks to be interrupted only once used idx passes 2.
        d.set_used_event(2);
        let mut notified = Vec::new();
        while let Some(c) = q.pop(&d.mem) {
            q.add_used(&d.mem, c.head, 0).unwrap();
            notified.push(q.needs_notification(&d.mem));
        }
        assert_eq!(notified, vec![false, false, true]);
        // avail_event mirrors how far we consumed.
        let ae: u16 = d.mem.read_obj(q.used_ring.unchecked_add(4 + 8 * 8)).unwrap();
        assert_eq!(ae, 3);
    }

    #[test]
    fn malformed_chain_is_skipped() {
        let mut d = Driver::new(4);
        // Point a descriptor outside guest RAM.
        let (h, _) = d.add_chain(&[b"ok"], &[]);
        d.mem.write_obj(0x10u64, d.q.desc_table.unchecked_add(h as u64 * 16)).unwrap();
        let (h2, _) = d.add_chain(&[b"good"], &[]);
        let mut q = d.q.clone();
        let c = q.pop(&d.mem).unwrap();
        assert_eq!(c.head, h2);
        assert_eq!(d.take_used(), vec![(h, 0)]);
    }

    #[test]
    fn descriptor_loop_detected() {
        let mut d = Driver::new(4);
        let (h, _) = d.add_chain(&[b"a", b"b"], &[]);
        // Make the second descriptor point back to the first.
        let second = d.q.desc_table.unchecked_add(((h + 1) % 4) as u64 * 16);
        d.mem.write_obj(DESC_F_NEXT, second.unchecked_add(12)).unwrap();
        d.mem.write_obj(h, second.unchecked_add(14)).unwrap();
        let mut q = d.q.clone();
        assert!(q.pop(&d.mem).is_none());
        assert_eq!(d.take_used(), vec![(h, 0)]);
    }
}
