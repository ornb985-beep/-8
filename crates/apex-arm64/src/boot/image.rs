//! arm64 Linux `Image` header (Documentation/arch/arm64/booting.rst) and
//! transparent decompression of `Image.gz` / `Image.lz4`.

use apex_core::{Error, Result};

use super::{inflate, lz4};

pub const ARM64_MAGIC: u32 = 0x644d_5241; // "ARM\x64"
pub const LEGACY_TEXT_OFFSET: u64 = 0x8_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageHeader {
    pub text_offset: u64,
    /// Effective image size including BSS; 0 in pre-3.17 kernels.
    pub image_size: u64,
    pub flags: u64,
}

impl ImageHeader {
    pub fn parse(data: &[u8]) -> Result<ImageHeader> {
        if data.len() < 64 {
            return Err(Error::Boot("kernel image too small".into()));
        }
        let u64_at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        let magic = u32::from_le_bytes(data[56..60].try_into().unwrap());
        if magic != ARM64_MAGIC {
            return Err(Error::Boot("not an arm64 Linux Image (bad magic)".into()));
        }
        let mut h = ImageHeader { text_offset: u64_at(8), image_size: u64_at(16), flags: u64_at(24) };
        if h.image_size == 0 {
            h.text_offset = LEGACY_TEXT_OFFSET;
        }
        if h.flags & 1 != 0 {
            return Err(Error::Boot("big-endian kernels are not supported".into()));
        }
        Ok(h)
    }

    /// Memory the kernel needs from its load address (at least the file).
    pub fn footprint(&self, file_len: usize) -> u64 {
        self.image_size.max(file_len as u64)
    }

    /// Kernel page size advertised in flags bits 1-2 (0 = unspecified).
    pub fn page_size(&self) -> Option<u64> {
        match (self.flags >> 1) & 3 {
            1 => Some(4096),
            2 => Some(16384),
            3 => Some(65536),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Lz4Legacy,
    Lz4Frame,
}

pub fn detect_compression(data: &[u8]) -> Compression {
    match data {
        [0x1f, 0x8b, ..] => Compression::Gzip,
        [0x02, 0x21, 0x4c, 0x18, ..] => Compression::Lz4Legacy,
        [0x04, 0x22, 0x4d, 0x18, ..] => Compression::Lz4Frame,
        _ => Compression::None,
    }
}

/// Decompress (if needed) and validate a kernel image.
pub fn prepare_kernel(data: &[u8]) -> Result<(Vec<u8>, ImageHeader)> {
    let raw = match detect_compression(data) {
        Compression::None => data.to_vec(),
        Compression::Gzip => inflate::gunzip(data)?,
        Compression::Lz4Legacy => lz4::decompress_legacy(data)?,
        Compression::Lz4Frame => lz4::decompress_frame(data)?,
    };
    let hdr = ImageHeader::parse(&raw)?;
    Ok((raw, hdr))
}

#[cfg(test)]
pub(crate) fn fake_image(len: usize, text_offset: u64, image_size: u64) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[0..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // b #0x40
    v[8..16].copy_from_slice(&text_offset.to_le_bytes());
    v[16..24].copy_from_slice(&image_size.to_le_bytes());
    v[24..32].copy_from_slice(&(0b1010u64).to_le_bytes()); // 4K pages, anywhere
    v[56..60].copy_from_slice(&ARM64_MAGIC.to_le_bytes());
    for (i, b) in v.iter_mut().enumerate().skip(64) {
        *b = (i * 7) as u8;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header() {
        let img = fake_image(4096, 0, 0x20_0000);
        let h = ImageHeader::parse(&img).unwrap();
        assert_eq!(h.text_offset, 0);
        assert_eq!(h.image_size, 0x20_0000);
        assert_eq!(h.page_size(), Some(4096));
        assert_eq!(h.footprint(4096), 0x20_0000);
        let legacy = fake_image(4096, 0, 0);
        assert_eq!(ImageHeader::parse(&legacy).unwrap().text_offset, LEGACY_TEXT_OFFSET);
        let mut bad = img.clone();
        bad[56] = 0;
        assert!(ImageHeader::parse(&bad).is_err());
    }
}
