//! LZ4 decoders: the legacy container produced by the kernel's `Image.lz4`
//! build rule (and used for GKI ramdisks) and the standard LZ4 frame format.

use apex_core::{Error, Result};

const LEGACY_MAGIC: u32 = 0x184c_2102;
const FRAME_MAGIC: u32 = 0x184d_2204;
const LEGACY_BLOCK_MAX: usize = 8 << 20;

fn corrupt(msg: &str) -> Error {
    Error::Boot(format!("corrupt lz4 data: {msg}"))
}

/// Decode one LZ4 block, appending to `out`. Matches may reference data that
/// was already in `out` (linked blocks).
pub fn decompress_block(src: &[u8], out: &mut Vec<u8>, max_out: usize) -> Result<()> {
    let limit = out.len() + max_out;
    let mut i = 0;
    loop {
        let token = *src.get(i).ok_or_else(|| corrupt("missing token"))?;
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(i).ok_or_else(|| corrupt("literal length"))?;
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let lits = src.get(i..i + lit).ok_or_else(|| corrupt("literals overrun"))?;
        if out.len() + lit > limit {
            return Err(corrupt("output overrun"));
        }
        out.extend_from_slice(lits);
        i += lit;
        if i == src.len() {
            return Ok(()); // last sequence carries literals only
        }
        let off = u16::from_le_bytes(src.get(i..i + 2).ok_or_else(|| corrupt("offset"))?.try_into().unwrap()) as usize;
        i += 2;
        if off == 0 || off > out.len() {
            return Err(corrupt("bad match offset"));
        }
        let mut mlen = (token & 0xf) as usize;
        if mlen == 15 {
            loop {
                let b = *src.get(i).ok_or_else(|| corrupt("match length"))?;
                i += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        mlen += 4;
        if out.len() + mlen > limit {
            return Err(corrupt("output overrun"));
        }
        let start = out.len() - off;
        if off >= mlen {
            out.extend_from_within(start..start + mlen);
        } else {
            for k in 0..mlen {
                let v = out[start + k];
                out.push(v);
            }
        }
    }
}

fn le32(d: &[u8], i: usize) -> Option<u32> {
    d.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// Legacy format: magic followed by `[le32 size][block]` records, each block
/// decompressing to at most 8 MiB. Concatenated archives repeat the magic;
/// the kernel's `size_append` leaves 4 trailing bytes which are ignored.
pub fn decompress_legacy(data: &[u8]) -> Result<Vec<u8>> {
    if le32(data, 0) != Some(LEGACY_MAGIC) {
        return Err(corrupt("bad legacy magic"));
    }
    let mut out = Vec::with_capacity(data.len() * 3);
    let mut i = 4;
    while i < data.len() {
        let Some(size) = le32(data, i) else { break };
        if size == LEGACY_MAGIC {
            i += 4;
            continue;
        }
        let size = size as usize;
        let Some(block) = data.get(i + 4..i + 4 + size) else {
            // Trailing uncompressed-size word or padding.
            if data.len() - i <= 8 {
                break;
            }
            return Err(corrupt("truncated legacy block"));
        };
        // Blocks are independent in the legacy format, so each one is decoded
        // into a fresh window (offsets cannot reach into earlier blocks).
        let mut tmp = Vec::with_capacity(LEGACY_BLOCK_MAX.min(size * 4));
        decompress_block(block, &mut tmp, LEGACY_BLOCK_MAX)?;
        out.extend_from_slice(&tmp);
        i += 4 + size;
    }
    Ok(out)
}

/// XXH32 used by LZ4 frame checksums.
pub fn xxh32(data: &[u8], seed: u32) -> u32 {
    const P1: u32 = 2_654_435_761;
    const P2: u32 = 2_246_822_519;
    const P3: u32 = 3_266_489_917;
    const P4: u32 = 668_265_263;
    const P5: u32 = 374_761_393;
    let round = |acc: u32, lane: u32| acc.wrapping_add(lane.wrapping_mul(P2)).rotate_left(13).wrapping_mul(P1);
    let mut i = 0;
    let mut h: u32;
    if data.len() >= 16 {
        let mut v = [seed.wrapping_add(P1).wrapping_add(P2), seed.wrapping_add(P2), seed, seed.wrapping_sub(P1)];
        while i + 16 <= data.len() {
            for (k, lane) in v.iter_mut().enumerate() {
                *lane = round(*lane, le32(data, i + k * 4).unwrap());
            }
            i += 16;
        }
        h = v[0].rotate_left(1).wrapping_add(v[1].rotate_left(7)).wrapping_add(v[2].rotate_left(12)).wrapping_add(v[3].rotate_left(18));
    } else {
        h = seed.wrapping_add(P5);
    }
    h = h.wrapping_add(data.len() as u32);
    while i + 4 <= data.len() {
        h = h.wrapping_add(le32(data, i).unwrap().wrapping_mul(P3)).rotate_left(17).wrapping_mul(P4);
        i += 4;
    }
    while i < data.len() {
        h = h.wrapping_add((data[i] as u32).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
        i += 1;
    }
    h ^= h >> 15;
    h = h.wrapping_mul(P2);
    h ^= h >> 13;
    h = h.wrapping_mul(P3);
    h ^= h >> 16;
    h
}

/// Standard LZ4 frame format (lz4 CLI default), all frames in the input.
pub fn decompress_frame(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 3);
    let mut i = 0;
    while i < data.len() {
        let magic = le32(data, i).ok_or_else(|| corrupt("frame magic"))?;
        if (0x184d_2a50..=0x184d_2a5f).contains(&magic) {
            // Skippable frame.
            let len = le32(data, i + 4).ok_or_else(|| corrupt("skippable"))? as usize;
            i += 8 + len;
            continue;
        }
        if magic != FRAME_MAGIC {
            return Err(corrupt("bad frame magic"));
        }
        let desc_start = i + 4;
        let flg = *data.get(desc_start).ok_or_else(|| corrupt("FLG"))?;
        let bd = *data.get(desc_start + 1).ok_or_else(|| corrupt("BD"))?;
        if flg >> 6 != 1 {
            return Err(corrupt("unsupported frame version"));
        }
        let block_checksum = flg & 0x10 != 0;
        let content_size = flg & 0x08 != 0;
        let content_checksum = flg & 0x04 != 0;
        let dict_id = flg & 0x01 != 0;
        let block_max = match (bd >> 4) & 7 {
            4 => 64 << 10,
            5 => 256 << 10,
            6 => 1 << 20,
            7 => 4 << 20,
            _ => return Err(corrupt("bad block max size")),
        };
        let mut p = desc_start + 2;
        let mut expected_size = None;
        if content_size {
            expected_size = Some(u64::from_le_bytes(data.get(p..p + 8).ok_or_else(|| corrupt("content size"))?.try_into().unwrap()));
            p += 8;
        }
        if dict_id {
            return Err(corrupt("dictionaries are not supported"));
        }
        let hc = *data.get(p).ok_or_else(|| corrupt("HC"))?;
        let expect_hc = (xxh32(&data[desc_start..p], 0) >> 8) as u8;
        if hc != expect_hc {
            return Err(corrupt("frame header checksum"));
        }
        p += 1;
        let frame_start = out.len();
        loop {
            let word = le32(data, p).ok_or_else(|| corrupt("block size"))?;
            p += 4;
            if word == 0 {
                break;
            }
            let uncompressed = word & 0x8000_0000 != 0;
            let size = (word & 0x7fff_ffff) as usize;
            let block = data.get(p..p + size).ok_or_else(|| corrupt("truncated block"))?;
            if block_checksum {
                let c = le32(data, p + size).ok_or_else(|| corrupt("block checksum"))?;
                if c != xxh32(block, 0) {
                    return Err(corrupt("block checksum mismatch"));
                }
            }
            if uncompressed {
                out.extend_from_slice(block);
            } else {
                decompress_block(block, &mut out, block_max)?;
            }
            p += size + if block_checksum { 4 } else { 0 };
        }
        if content_checksum {
            let c = le32(data, p).ok_or_else(|| corrupt("content checksum"))?;
            if c != xxh32(&out[frame_start..], 0) {
                return Err(corrupt("content checksum mismatch"));
            }
            p += 4;
        }
        if let Some(sz) = expected_size {
            if (out.len() - frame_start) as u64 != sz {
                return Err(corrupt("content size mismatch"));
            }
        }
        i = p;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &[u8] = include_bytes!("../../testdata/payload.bin");

    #[test]
    fn legacy_kernel_style() {
        assert_eq!(decompress_legacy(include_bytes!("../../testdata/payload.lz4legacy")).unwrap(), PAYLOAD);
    }

    #[test]
    fn frame_with_checksums() {
        assert_eq!(decompress_frame(include_bytes!("../../testdata/payload.lz4")).unwrap(), PAYLOAD);
    }

    #[test]
    fn frame_corruption_detected() {
        let mut f = include_bytes!("../../testdata/payload.lz4").to_vec();
        let n = f.len();
        f[n / 2] ^= 0x55;
        assert!(decompress_frame(&f).is_err());
    }

    #[test]
    fn xxh32_vectors() {
        assert_eq!(xxh32(b"", 0), 0x02cc_5d05);
        assert_eq!(xxh32(b"abc", 0), 0x32d1_53ff);
        assert_eq!(xxh32(b"Nobody inspects the spammish repetition", 0), 0xe229_3b2f);
    }

    #[test]
    fn overlapping_match() {
        // literal "ab", then match offset 2 length 6 => "abababab", then "z"
        let block = [0x22, b'a', b'b', 0x02, 0x00, 0x10, b'z'];
        let mut out = Vec::new();
        decompress_block(&block, &mut out, 64).unwrap();
        assert_eq!(out, b"abababab".iter().chain(b"z").copied().collect::<Vec<_>>());
    }
}
