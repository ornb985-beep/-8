//! DEFLATE (RFC 1951) and gzip (RFC 1952) decoder for `Image.gz` kernels
//! and gzip-compressed ramdisks. Table driven, no dependencies.

use apex_core::{Error, Result};

const MAX_BITS: usize = 15;
const TABLE_BITS: u32 = 15;

const LBASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385,
    24577,
];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn corrupt(msg: &str) -> Error {
    Error::Boot(format!("corrupt deflate stream: {msg}"))
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
    /// Zero bits appended past the end of input (to detect truncation).
    overrun: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, cnt: 0, overrun: 0 }
    }

    #[inline]
    fn refill(&mut self) {
        while self.cnt <= 56 {
            let b = if self.pos < self.data.len() {
                let b = self.data[self.pos];
                self.pos += 1;
                b
            } else {
                self.overrun += 8;
                0
            };
            self.buf |= (b as u64) << self.cnt;
            self.cnt += 8;
        }
    }

    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.cnt < n {
            self.refill();
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    #[inline]
    fn consume(&mut self, n: u32) -> Result<()> {
        self.buf >>= n;
        self.cnt -= n;
        // Bits beyond the real input were consumed => truncated stream.
        if self.overrun > self.cnt {
            return Err(corrupt("unexpected end of input"));
        }
        Ok(())
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }

    fn align_byte(&mut self) {
        let drop = self.cnt % 8;
        self.buf >>= drop;
        self.cnt -= drop;
    }

    /// Byte position of the next unread input byte (after `align_byte`).
    fn byte_pos(&self) -> usize {
        let real_bits = self.cnt.saturating_sub(self.overrun);
        self.pos - (real_bits / 8) as usize
    }

    /// Discard the bit buffer and continue reading at byte offset `p`.
    fn reset_to(&mut self, p: usize) {
        self.pos = p;
        self.buf = 0;
        self.cnt = 0;
        self.overrun = 0;
    }
}

/// Canonical Huffman decoding table indexed by the next `TABLE_BITS` input
/// bits (LSB first). Entry = symbol << 4 | code length; length 0 = invalid.
struct Table {
    entries: Vec<u16>,
}

impl Table {
    fn build(lengths: &[u8]) -> Result<Table> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(corrupt("over-subscribed code"));
            }
        }
        let mut next = [0u32; MAX_BITS + 2];
        let mut code = 0u32;
        for bits in 1..=MAX_BITS {
            code = (code + count[bits - 1] as u32) << 1;
            next[bits] = code;
        }
        let mut entries = vec![0u16; 1 << TABLE_BITS];
        for (sym, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let len = len as u32;
            let c = next[len as usize];
            next[len as usize] += 1;
            let rev = c.reverse_bits() >> (32 - len);
            let e = ((sym as u16) << 4) | len as u16;
            let mut i = rev;
            while i < (1 << TABLE_BITS) {
                entries[i as usize] = e;
                i += 1 << len;
            }
        }
        Ok(Table { entries })
    }

    #[inline]
    fn decode(&self, b: &mut Bits) -> Result<u16> {
        let e = self.entries[b.peek(TABLE_BITS) as usize];
        let len = (e & 0xf) as u32;
        if len == 0 {
            return Err(corrupt("invalid Huffman code"));
        }
        b.consume(len)?;
        Ok(e >> 4)
    }
}

fn fixed_tables() -> (Table, Table) {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    (Table::build(&l).unwrap(), Table::build(&[5u8; 30]).unwrap())
}

fn dynamic_tables(b: &mut Bits) -> Result<(Table, Table)> {
    let nlen = b.bits(5)? as usize + 257;
    let ndist = b.bits(5)? as usize + 1;
    let ncode = b.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(corrupt("bad code counts"));
    }
    let mut cl = [0u8; 19];
    for &idx in CL_ORDER.iter().take(ncode) {
        cl[idx] = b.bits(3)? as u8;
    }
    let clt = Table::build(&cl)?;
    let mut lengths = vec![0u8; nlen + ndist];
    let mut i = 0;
    while i < nlen + ndist {
        let sym = clt.decode(b)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(corrupt("repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let n = 3 + b.bits(2)? as usize;
                if i + n > lengths.len() {
                    return Err(corrupt("too many lengths"));
                }
                lengths[i..i + n].fill(prev);
                i += n;
            }
            17 | 18 => {
                let n = if sym == 17 { 3 + b.bits(3)? as usize } else { 11 + b.bits(7)? as usize };
                if i + n > lengths.len() {
                    return Err(corrupt("too many lengths"));
                }
                i += n; // already zero
            }
            _ => return Err(corrupt("bad code length symbol")),
        }
    }
    if lengths[256] == 0 {
        return Err(corrupt("no end-of-block code"));
    }
    Ok((Table::build(&lengths[..nlen])?, Table::build(&lengths[nlen..])?))
}

fn codes(b: &mut Bits, out: &mut Vec<u8>, lit: &Table, dist: &Table, limit: usize) -> Result<()> {
    loop {
        let sym = lit.decode(b)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let s = sym - 257;
            if s >= 29 {
                return Err(corrupt("bad length symbol"));
            }
            let len = LBASE[s] as usize + b.bits(LEXT[s] as u32)? as usize;
            let ds = dist.decode(b)? as usize;
            if ds >= 30 {
                return Err(corrupt("bad distance symbol"));
            }
            let d = DBASE[ds] as usize + b.bits(DEXT[ds] as u32)? as usize;
            if d > out.len() {
                return Err(corrupt("distance too far back"));
            }
            if out.len() + len > limit {
                return Err(corrupt("output exceeds limit"));
            }
            let start = out.len() - d;
            if d >= len {
                out.extend_from_within(start..start + len);
            } else {
                for k in 0..len {
                    let v = out[start + k];
                    out.push(v);
                }
            }
        }
        if out.len() > limit {
            return Err(corrupt("output exceeds limit"));
        }
    }
}

/// Inflate a raw DEFLATE stream. Returns the number of input bytes consumed.
pub fn inflate_raw(data: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize> {
    let mut b = Bits::new(data);
    let fixed = std::cell::OnceCell::new();
    loop {
        let last = b.bits(1)? == 1;
        match b.bits(2)? {
            0 => {
                b.align_byte();
                let p = b.byte_pos();
                let hdr = data.get(p..p + 4).ok_or_else(|| corrupt("truncated stored block"))?;
                let len = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
                let nlen = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
                if len != !nlen & 0xffff {
                    return Err(corrupt("stored block length mismatch"));
                }
                let src = data.get(p + 4..p + 4 + len).ok_or_else(|| corrupt("truncated stored block"))?;
                if out.len() + len > limit {
                    return Err(corrupt("output exceeds limit"));
                }
                out.extend_from_slice(src);
                b.reset_to(p + 4 + len);
            }
            1 => {
                let (l, d) = fixed.get_or_init(fixed_tables);
                codes(&mut b, out, l, d, limit)?;
            }
            2 => {
                let (l, d) = dynamic_tables(&mut b)?;
                codes(&mut b, out, &l, &d, limit)?;
            }
            _ => return Err(corrupt("reserved block type")),
        }
        if out.len() > limit {
            return Err(corrupt("output exceeds limit"));
        }
        if last {
            b.align_byte();
            return Ok(b.byte_pos());
        }
    }
}

pub use apex_core::crc::crc32;

/// Maximum decompressed size accepted (guards against decompression bombs).
pub const MAX_OUTPUT: usize = 1 << 31;

/// Decompress a gzip file (all concatenated members), verifying CRC32/ISIZE.
pub fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4);
    let mut p = 0;
    while p < data.len() {
        let hdr = data.get(p..p + 10).ok_or_else(|| corrupt("truncated gzip header"))?;
        if hdr[0] != 0x1f || hdr[1] != 0x8b {
            // Trailing padding (e.g. zeros appended by image tools) is ignored.
            if data[p..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(Error::Boot("bad gzip magic".into()));
        }
        if hdr[2] != 8 {
            return Err(Error::Boot("gzip: unsupported compression method".into()));
        }
        let flg = hdr[3];
        let mut q = p + 10;
        if flg & 0x04 != 0 {
            let xlen = u16::from_le_bytes(data.get(q..q + 2).ok_or_else(|| corrupt("FEXTRA"))?.try_into().unwrap()) as usize;
            q += 2 + xlen;
        }
        for flag in [0x08u8, 0x10] {
            if flg & flag != 0 {
                let z = data.get(q..).and_then(|s| s.iter().position(|&b| b == 0)).ok_or_else(|| corrupt("FNAME"))?;
                q += z + 1;
            }
        }
        if flg & 0x02 != 0 {
            q += 2;
        }
        let start = out.len();
        let body = data.get(q..).ok_or_else(|| corrupt("truncated gzip"))?;
        let used = inflate_raw(body, &mut out, MAX_OUTPUT)?;
        q += used;
        let trailer = data.get(q..q + 8).ok_or_else(|| corrupt("missing gzip trailer"))?;
        let crc = u32::from_le_bytes(trailer[0..4].try_into().unwrap());
        let isize = u32::from_le_bytes(trailer[4..8].try_into().unwrap());
        if crc32(&out[start..]) != crc {
            return Err(Error::Boot("gzip CRC mismatch".into()));
        }
        if (out.len() - start) as u32 != isize {
            return Err(Error::Boot("gzip size mismatch".into()));
        }
        p = q + 8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &[u8] = include_bytes!("../../testdata/payload.bin");

    #[test]
    fn gzip_dynamic() {
        assert_eq!(gunzip(include_bytes!("../../testdata/payload.gz")).unwrap(), PAYLOAD);
    }

    #[test]
    fn gzip_stored() {
        assert_eq!(gunzip(include_bytes!("../../testdata/payload.stored.gz")).unwrap(), PAYLOAD);
    }

    #[test]
    fn raw_fixed_huffman() {
        let mut out = Vec::new();
        let data = include_bytes!("../../testdata/payload.fixed.deflate");
        let used = inflate_raw(data, &mut out, MAX_OUTPUT).unwrap();
        assert_eq!(used, data.len());
        assert_eq!(out, PAYLOAD);
    }

    #[test]
    fn detects_corruption() {
        let mut gz = include_bytes!("../../testdata/payload.gz").to_vec();
        let n = gz.len();
        assert!(gunzip(&gz[..n / 2]).is_err());
        gz[n - 6] ^= 0xff; // CRC byte
        assert!(gunzip(&gz).is_err());
        assert!(gunzip(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\xff\xff").is_err());
    }

    #[test]
    fn crc_known_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
