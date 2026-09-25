//! CRC-32 (IEEE 802.3, reflected, as used by gzip and GPT).

fn table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    })
}

/// Continue a CRC over more data (start with `crc = 0`).
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let t = table();
    let mut c = !crc;
    for &b in data {
        c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        assert_eq!(super::crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(super::crc32_update(super::crc32(b"1234"), b"56789"), 0xcbf4_3926);
    }
}
