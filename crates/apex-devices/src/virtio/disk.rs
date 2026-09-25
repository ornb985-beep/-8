//! Disk back ends for virtio-blk.
//!
//! * [`RawDisk`] — a plain image file.
//! * [`CompositeDisk`] — several image files presented as one disk with a
//!   synthesized GPT, so Android's first-stage init finds
//!   `/dev/block/by-name/{super,userdata,metadata,misc,...}` exactly as on a
//!   real phone's UFS/eMMC. The partition table itself is generated in
//!   memory; only partition contents live in files.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use apex_core::crc::crc32;
use apex_core::{align_up, sys};

pub const SECTOR: u64 = 512;

pub trait DiskBackend: Send + Sync {
    fn size(&self) -> u64;
    fn read_only(&self) -> bool;
    /// Read exactly `buf.len()` bytes; ranges past the end of the backing
    /// file read as zeros.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn flush(&self) -> io::Result<()>;
    /// Advisory deallocation.
    fn discard(&self, offset: u64, len: u64) -> io::Result<()>;
    fn write_zeroes(&self, offset: u64, len: u64) -> io::Result<()> {
        let zeros = vec![0u8; (len.min(1 << 20)) as usize];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(zeros.len() as u64) as usize;
            self.write_at(&zeros[..n], offset + done)?;
            done += n as u64;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    /// FLUSH is forwarded to the host (fsync).
    Writeback,
    /// FLUSH is ignored: fastest, data loss on host crash.
    Unsafe,
}

pub struct RawDisk {
    file: File,
    size: u64,
    read_only: bool,
    cache: CacheMode,
    punch_ok: AtomicBool,
}

impl RawDisk {
    pub fn open(path: &Path, read_only: bool, cache: CacheMode) -> io::Result<RawDisk> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(RawDisk { file, size, read_only, cache, punch_ok: AtomicBool::new(true) })
    }

    pub fn from_file(file: File, read_only: bool) -> io::Result<RawDisk> {
        let size = file.metadata()?.len();
        Ok(RawDisk { file, size, read_only, cache: CacheMode::Writeback, punch_ok: AtomicBool::new(true) })
    }
}

impl DiskBackend for RawDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => {
                    buf[done..].fill(0);
                    break;
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "write beyond end of disk"));
        }
        self.file.write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        match self.cache {
            CacheMode::Writeback if !self.read_only => self.file.sync_data(),
            _ => Ok(()),
        }
    }
    fn discard(&self, offset: u64, len: u64) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if !self.punch_ok.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Only whole 4 KiB blocks can be deallocated.
        let start = align_up(offset, 4096);
        let end = (offset + len) & !4095;
        if end <= start {
            return Ok(());
        }
        if let Err(e) = sys::punch_hole(self.file.as_raw_fd(), start, end - start) {
            // File system without hole punching: discard is only a hint.
            apex_core::debug!("punch_hole unsupported ({e}), discards become no-ops");
            self.punch_ok.store(false, Ordering::Relaxed);
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// GPT composite disk
// ----------------------------------------------------------------------

const GPT_ENTRIES: usize = 128;
const GPT_ENTRY_SIZE: usize = 128;
const GPT_ENTRY_LBAS: u64 = (GPT_ENTRIES * GPT_ENTRY_SIZE) as u64 / SECTOR; // 32
const PART_ALIGN_LBAS: u64 = 2048; // 1 MiB

/// Linux filesystem data type GUID 0FC63DAF-8483-4772-8E79-3D69D8477DE4.
const TYPE_LINUX_DATA: [u8; 16] = [0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4];

pub struct PartitionSpec {
    pub name: String,
    pub backend: Box<dyn DiskBackend>,
}

struct Partition {
    name: String,
    start: u64, // bytes
    len: u64,   // bytes (LBA aligned)
    backend: Box<dyn DiskBackend>,
}

pub struct CompositeDisk {
    head: Vec<u8>, // LBA 0..34
    tail: Vec<u8>, // backup entries + header
    tail_start: u64,
    parts: Vec<Partition>,
    size: u64,
    read_only: bool,
    warned: AtomicBool,
}

/// Deterministic RFC 4122 v4-shaped GUID from a string (stable across boots).
pub fn guid_from(seed: &str) -> [u8; 16] {
    let fnv = |s: u64| {
        let mut h = 0xcbf2_9ce4_8422_2325u64 ^ s;
        for b in seed.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    };
    let mut g = [0u8; 16];
    g[..8].copy_from_slice(&fnv(0x61706578).to_le_bytes());
    g[8..].copy_from_slice(&fnv(0x676f6f67).to_le_bytes());
    g[7] = (g[7] & 0x0f) | 0x40; // version 4 (time_hi_and_version is LE in GPT)
    g[8] = (g[8] & 0x3f) | 0x80; // RFC 4122 variant
    g
}

impl CompositeDisk {
    pub fn new(disk_name: &str, specs: Vec<PartitionSpec>) -> io::Result<CompositeDisk> {
        if specs.is_empty() || specs.len() > GPT_ENTRIES {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "composite disk needs 1..128 partitions"));
        }
        let mut parts = Vec::new();
        let mut lba = PART_ALIGN_LBAS;
        let mut read_only = true;
        for s in specs {
            if s.name.is_empty() || s.name.encode_utf16().count() > 36 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad partition name `{}`", s.name)));
            }
            let len = align_up(s.backend.size().max(SECTOR), SECTOR);
            read_only &= s.backend.read_only();
            parts.push(Partition { name: s.name, start: lba * SECTOR, len, backend: s.backend });
            lba = align_up(lba + len / SECTOR, PART_ALIGN_LBAS);
        }
        let total_lbas = lba + GPT_ENTRY_LBAS + 2;
        let last_lba = total_lbas - 1;
        let first_usable = 2 + GPT_ENTRY_LBAS;
        let last_usable = total_lbas - GPT_ENTRY_LBAS - 2;

        let mut entries = vec![0u8; GPT_ENTRIES * GPT_ENTRY_SIZE];
        for (i, p) in parts.iter().enumerate() {
            let e = &mut entries[i * GPT_ENTRY_SIZE..(i + 1) * GPT_ENTRY_SIZE];
            e[0..16].copy_from_slice(&TYPE_LINUX_DATA);
            e[16..32].copy_from_slice(&guid_from(&format!("{disk_name}/{}", p.name)));
            e[32..40].copy_from_slice(&(p.start / SECTOR).to_le_bytes());
            e[40..48].copy_from_slice(&((p.start + p.len) / SECTOR - 1).to_le_bytes());
            for (k, u) in p.name.encode_utf16().enumerate() {
                e[56 + 2 * k..58 + 2 * k].copy_from_slice(&u.to_le_bytes());
            }
        }
        let entries_crc = crc32(&entries);
        let disk_guid = guid_from(disk_name);
        let header = |current: u64, backup: u64, entries_lba: u64| {
            let mut h = vec![0u8; SECTOR as usize];
            h[0..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&current.to_le_bytes());
            h[32..40].copy_from_slice(&backup.to_le_bytes());
            h[40..48].copy_from_slice(&first_usable.to_le_bytes());
            h[48..56].copy_from_slice(&last_usable.to_le_bytes());
            h[56..72].copy_from_slice(&disk_guid);
            h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            h[80..84].copy_from_slice(&(GPT_ENTRIES as u32).to_le_bytes());
            h[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
            h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let c = crc32(&h[..92]);
            h[16..20].copy_from_slice(&c.to_le_bytes());
            h
        };

        let mut head = vec![0u8; ((2 + GPT_ENTRY_LBAS) * SECTOR) as usize];
        // Protective MBR
        let mbr = &mut head[446..462];
        mbr[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
        mbr[4] = 0xee;
        mbr[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
        mbr[8..12].copy_from_slice(&1u32.to_le_bytes());
        mbr[12..16].copy_from_slice(&((last_lba).min(0xffff_ffff) as u32).to_le_bytes());
        head[510] = 0x55;
        head[511] = 0xaa;
        head[512..1024].copy_from_slice(&header(1, last_lba, 2));
        head[1024..1024 + entries.len()].copy_from_slice(&entries);

        let tail_start = (last_lba - GPT_ENTRY_LBAS) * SECTOR;
        let mut tail = entries.clone();
        tail.extend_from_slice(&header(last_lba, 1, last_lba - GPT_ENTRY_LBAS));

        Ok(CompositeDisk { head, tail, tail_start, parts, size: total_lbas * SECTOR, read_only, warned: AtomicBool::new(false) })
    }

    pub fn partitions(&self) -> impl Iterator<Item = (&str, u64, u64)> {
        self.parts.iter().map(|p| (p.name.as_str(), p.start, p.len))
    }

    /// Split an I/O into the pieces it touches.
    fn for_each<F>(&self, offset: u64, len: u64, mut f: F) -> io::Result<()>
    where
        F: FnMut(Region<'_>, u64, u64, u64) -> io::Result<()>,
    {
        if offset + len > self.size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "I/O beyond end of composite disk"));
        }
        let mut pos = offset;
        let end = offset + len;
        while pos < end {
            let (region, rstart, rend) = self.region_at(pos);
            let n = rend.min(end) - pos;
            f(region, pos - rstart, pos - offset, n)?;
            pos += n;
        }
        Ok(())
    }

    /// Region containing byte `pos`: (region, start, end).
    fn region_at(&self, pos: u64) -> (Region<'_>, u64, u64) {
        let head_len = self.head.len() as u64;
        if pos < head_len {
            return (Region::Head, 0, head_len);
        }
        if pos >= self.tail_start {
            return (Region::Tail, self.tail_start, self.size);
        }
        let mut gap_start = head_len;
        for p in &self.parts {
            if pos < p.start {
                return (Region::Gap, gap_start, p.start);
            }
            if pos < p.start + p.len {
                return (Region::Part(p), p.start, p.start + p.len);
            }
            gap_start = p.start + p.len;
        }
        (Region::Gap, gap_start, self.tail_start)
    }
}

enum Region<'a> {
    Head,
    Tail,
    Gap,
    Part(&'a Partition),
}

impl DiskBackend for CompositeDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.for_each(offset, buf.len() as u64, |r, roff, boff, n| {
            let dst = &mut buf[boff as usize..(boff + n) as usize];
            match r {
                Region::Head => dst.copy_from_slice(&self.head[roff as usize..(roff + n) as usize]),
                Region::Tail => dst.copy_from_slice(&self.tail[roff as usize..(roff + n) as usize]),
                Region::Gap => dst.fill(0),
                Region::Part(p) => p.backend.read_at(dst, roff)?,
            }
            Ok(())
        })
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.for_each(offset, buf.len() as u64, |r, roff, boff, n| match r {
            Region::Part(p) => {
                let src = &buf[boff as usize..(boff + n) as usize];
                // Writes into the LBA padding after a short backing file are dropped.
                let file_len = p.backend.size();
                if roff >= file_len {
                    return Ok(());
                }
                let m = (file_len - roff).min(n) as usize;
                p.backend.write_at(&src[..m], roff)
            }
            _ => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    apex_core::warn!("guest wrote to the synthesized partition table; ignored");
                }
                Ok(())
            }
        })
    }
    fn flush(&self) -> io::Result<()> {
        for p in &self.parts {
            if !p.backend.read_only() {
                p.backend.flush()?;
            }
        }
        Ok(())
    }
    fn discard(&self, offset: u64, len: u64) -> io::Result<()> {
        self.for_each(offset, len, |r, roff, _, n| match r {
            Region::Part(p) if !p.backend.read_only() => p.backend.discard(roff, n),
            _ => Ok(()),
        })
    }
    fn write_zeroes(&self, offset: u64, len: u64) -> io::Result<()> {
        self.for_each(offset, len, |r, roff, _, n| match r {
            Region::Part(p) => p.backend.write_zeroes(roff, n.min(p.backend.size().saturating_sub(roff))),
            _ => Ok(()),
        })
    }
}

/// A read-only all-zero disk of a given size (empty partitions in
/// `apex mkdisk`, which leaves them as holes in a sparse file).
pub struct ZeroDisk(pub u64);

impl DiskBackend for ZeroDisk {
    fn size(&self) -> u64 {
        self.0
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn discard(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }
}

/// Write any disk to a raw image file, leaving all-zero 1 MiB chunks as
/// holes (the result is sparse on APFS/ext4). Returns bytes of data written.
pub fn write_raw_image(disk: &dyn DiskBackend, out: &std::path::Path) -> io::Result<u64> {
    let f = File::create(out)?;
    let size = disk.size();
    f.set_len(size)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    let mut written = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(buf.len());
        disk.read_at(&mut buf[..n], off)?;
        if buf[..n].iter().any(|&b| b != 0) {
            f.write_all_at(&buf[..n], off)?;
            written += n as u64;
        }
        off += n as u64;
    }
    f.sync_all()?;
    Ok(written)
}

/// In-memory disk used by tests and for small synthesized images.
pub struct MemDisk {
    data: std::sync::RwLock<Vec<u8>>,
    read_only: bool,
}

impl MemDisk {
    pub fn new(data: Vec<u8>, read_only: bool) -> MemDisk {
        MemDisk { data: std::sync::RwLock::new(data), read_only }
    }
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.read().unwrap().clone()
    }
}

impl DiskBackend for MemDisk {
    fn size(&self) -> u64 {
        self.data.read().unwrap().len() as u64
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let d = self.data.read().unwrap();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = d.get(offset as usize + i).copied().unwrap_or(0);
        }
        Ok(())
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let mut d = self.data.write().unwrap();
        let end = offset as usize + buf.len();
        if end > d.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "write beyond end"));
        }
        d[offset as usize..end].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn discard(&self, _offset: u64, _len: u64) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composite() -> CompositeDisk {
        CompositeDisk::new(
            "apex-os",
            vec![
                PartitionSpec { name: "misc".into(), backend: Box::new(MemDisk::new(vec![0x11; 4096], false)) },
                PartitionSpec { name: "super".into(), backend: Box::new(MemDisk::new(vec![0x22; 3 << 20], true)) },
                PartitionSpec { name: "userdata".into(), backend: Box::new(MemDisk::new(vec![0; 1000], false)) },
            ],
        )
        .unwrap()
    }

    #[test]
    fn gpt_headers_are_valid() {
        let d = composite();
        let mut lba0 = vec![0u8; 512 * 34];
        d.read_at(&mut lba0, 0).unwrap();
        assert_eq!(&lba0[510..512], &[0x55, 0xaa]);
        assert_eq!(lba0[446 + 4], 0xee);
        let h = &lba0[512..1024];
        assert_eq!(&h[0..8], b"EFI PART");
        let mut hh = h[..92].to_vec();
        let stored = u32::from_le_bytes(hh[16..20].try_into().unwrap());
        hh[16..20].fill(0);
        assert_eq!(crc32(&hh), stored);
        let entries = &lba0[1024..1024 + 128 * 128];
        assert_eq!(crc32(entries), u32::from_le_bytes(h[88..92].try_into().unwrap()));
        // First entry: misc at LBA 2048, one sector... 8 sectors long.
        assert_eq!(u64::from_le_bytes(entries[32..40].try_into().unwrap()), 2048);
        assert_eq!(u64::from_le_bytes(entries[40..48].try_into().unwrap()), 2048 + 8 - 1);
        let name: String =
            char::decode_utf16(entries[56..64].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]))).map(|c| c.unwrap()).collect();
        assert_eq!(name, "misc");
        // Backup header at the last LBA points back to the primary.
        let last = d.size() / 512 - 1;
        let mut bh = vec![0u8; 512];
        d.read_at(&mut bh, last * 512).unwrap();
        assert_eq!(&bh[0..8], b"EFI PART");
        assert_eq!(u64::from_le_bytes(bh[24..32].try_into().unwrap()), last);
        assert_eq!(u64::from_le_bytes(bh[32..40].try_into().unwrap()), 1);
    }

    #[test]
    fn partition_io_routes_to_backends() {
        let d = composite();
        let parts: Vec<_> = d.partitions().map(|(n, s, l)| (n.to_string(), s, l)).collect();
        assert_eq!(parts[1].0, "super");
        let (_, super_start, super_len) = parts[1];
        let mut b = vec![0u8; 8192];
        // Straddle the gap before `super` and its first bytes.
        d.read_at(&mut b, super_start - 4096).unwrap();
        assert!(b[..4096].iter().all(|&x| x == 0));
        assert!(b[4096..].iter().all(|&x| x == 0x22));
        assert_eq!(super_len, 3 << 20);
        // userdata is writable, super is read-only.
        let (_, ud_start, ud_len) = parts[2];
        assert_eq!(ud_len, 1024);
        d.write_at(&[7u8; 512], ud_start).unwrap();
        d.read_at(&mut b[..4], ud_start).unwrap();
        assert_eq!(&b[..4], &[7, 7, 7, 7]);
        assert!(d.write_at(&[1u8; 512], super_start).is_err());
        // Writes to the synthesized header are ignored, not errors.
        d.write_at(&[0u8; 512], 0).unwrap();
        let mut sig = [0u8; 8];
        d.read_at(&mut sig, 512).unwrap();
        assert_eq!(&sig, b"EFI PART");
        assert!(d.read_at(&mut b, d.size() - 10).is_err());
    }

    #[test]
    fn raw_image_matches_composite_and_is_sparse() {
        let d = CompositeDisk::new(
            "t",
            vec![
                PartitionSpec { name: "system".into(), backend: Box::new(MemDisk::new(vec![0x5a; 8192], true)) },
                PartitionSpec { name: "userdata".into(), backend: Box::new(ZeroDisk(64 << 20)) },
            ],
        )
        .unwrap();
        let p = std::env::temp_dir().join(format!("apex-raw-{}.img", std::process::id()));
        let written = write_raw_image(&d, &p).unwrap();
        let raw = RawDisk::open(&p, true, CacheMode::Unsafe).unwrap();
        assert_eq!(raw.size(), d.size());
        assert!(written < 4 << 20, "userdata must stay a hole ({written} bytes written)");
        let (mut a, mut b) = (vec![0u8; 4096], vec![0u8; 4096]);
        for off in [0u64, 512, 2048 * 512, d.size() - 4096] {
            d.read_at(&mut a, off).unwrap();
            raw.read_at(&mut b, off).unwrap();
            assert_eq!(a, b, "offset {off:#x}");
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn guids_are_stable_and_distinct() {
        assert_eq!(guid_from("a"), guid_from("a"));
        assert_ne!(guid_from("a"), guid_from("b"));
        assert_eq!(guid_from("x")[7] >> 4, 4);
    }
}
