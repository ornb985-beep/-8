//! Linux bootconfig (Documentation/admin-guide/bootconfig.rst) as used by
//! Android 12+ to carry `androidboot.*` properties.
//!
//! Layout appended to the initrd:
//! `[text][NUL padding to 4][le32 size][le32 checksum]["#BOOTCONFIG\n"]`

use apex_core::{Error, Result};

pub const MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";
/// XBC_DATA_MAX in the kernel.
pub const MAX_SIZE: usize = 32767;

fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32))
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.split('.').all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Merge the vendor bootconfig text with VMM-provided parameters. The VMM
/// values win on conflicts (the kernel rejects duplicate keys).
pub fn merge(vendor: &[u8], params: &[(String, String)]) -> Result<Vec<u8>> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let text = String::from_utf8_lossy(vendor);
    for raw in text.lines() {
        let line = raw.trim().trim_end_matches('\0');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| Error::Boot(format!("bad bootconfig line `{line}`")))?;
        let k = k.trim().trim_end_matches(['+', ':']).trim();
        let v = v.trim().trim_matches('"');
        entries.retain(|(ek, _)| ek != k);
        entries.push((k.to_string(), v.to_string()));
    }
    for (k, v) in params {
        entries.retain(|(ek, _)| ek != k);
        entries.push((k.clone(), v.clone()));
    }
    let mut out = String::new();
    for (k, v) in &entries {
        if !valid_key(k) {
            return Err(Error::Boot(format!("invalid bootconfig key `{k}`")));
        }
        if v.contains(['"', '\n', '\0']) {
            return Err(Error::Boot(format!("invalid character in bootconfig value for `{k}`")));
        }
        out.push_str(&format!("{k} = \"{v}\"\n"));
    }
    if out.len() > MAX_SIZE {
        return Err(Error::Boot(format!("bootconfig is {} bytes, limit is {MAX_SIZE}", out.len())));
    }
    Ok(out.into_bytes())
}

/// Append a bootconfig block to `initrd`.
pub fn append(initrd: &mut Vec<u8>, text: &[u8]) -> Result<()> {
    if text.len() > MAX_SIZE {
        return Err(Error::Boot("bootconfig too large".into()));
    }
    let mut data = text.to_vec();
    // The kernel wants the trailer 4-byte aligned relative to the start of
    // the initrd (tools/bootconfig pads the same way).
    while (initrd.len() + data.len()) % 4 != 0 {
        data.push(0);
    }
    let size = data.len() as u32;
    let csum = checksum(&data);
    initrd.extend_from_slice(&data);
    initrd.extend_from_slice(&size.to_le_bytes());
    initrd.extend_from_slice(&csum.to_le_bytes());
    initrd.extend_from_slice(MAGIC);
    Ok(())
}

/// Locate and validate a bootconfig trailer; returns (offset, text).
pub fn find_trailer(initrd: &[u8]) -> Option<(usize, &[u8])> {
    let n = initrd.len();
    if n < 20 || &initrd[n - 12..] != MAGIC {
        return None;
    }
    let size = u32::from_le_bytes(initrd[n - 20..n - 16].try_into().unwrap()) as usize;
    let csum = u32::from_le_bytes(initrd[n - 16..n - 12].try_into().unwrap());
    let start = (n - 20).checked_sub(size)?;
    let data = &initrd[start..n - 20];
    if checksum(data) != csum {
        return None;
    }
    let text_end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    Some((start, &data[..text_end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_roundtrip_and_alignment() {
        for pre in 0..4 {
            let mut initrd = vec![0xaa; 100 + pre];
            let text = merge(b"", &[("androidboot.hardware".into(), "apex".into())]).unwrap();
            append(&mut initrd, &text).unwrap();
            assert_eq!((initrd.len() - 20) % 4, 0);
            let (start, t) = find_trailer(&initrd).unwrap();
            assert_eq!(start, 100 + pre);
            assert_eq!(t, b"androidboot.hardware = \"apex\"\n");
        }
    }

    #[test]
    fn merge_overrides_and_validates() {
        let t = merge(b"androidboot.a=1\n# c\nandroidboot.b = \"x y\"\n", &[("androidboot.a".into(), "2".into())]).unwrap();
        let s = String::from_utf8(t).unwrap();
        assert_eq!(s, "androidboot.b = \"x y\"\nandroidboot.a = \"2\"\n");
        assert!(merge(b"", &[("bad key".into(), "v".into())]).is_err());
        assert!(merge(b"", &[("k".into(), "a\"b".into())]).is_err());
        assert!(merge(b"garbage-line\n", &[]).is_err());
    }

    #[test]
    fn corrupt_checksum_rejected() {
        let mut initrd = Vec::new();
        append(&mut initrd, b"a = \"1\"\n").unwrap();
        initrd[0] ^= 1;
        assert!(find_trailer(&initrd).is_none());
    }
}
