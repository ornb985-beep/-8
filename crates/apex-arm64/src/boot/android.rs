//! Android boot image formats: `boot.img` (header v0-v4), `init_boot.img`
//! (v4, generic ramdisk only) and `vendor_boot.img` (v3/v4).
//!
//! The VMM plays the role of the bootloader (ABL/U-Boot/GBL): it extracts
//! the GKI kernel, concatenates vendor + generic ramdisks, merges kernel
//! command lines and appends the bootconfig block, exactly as described in
//! source.android.com "Boot image header" / "Implement Bootconfig".

use apex_core::{Error, Result};

use super::bootconfig;

pub const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
pub const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
const V3_PAGE: usize = 4096;

fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| Error::Boot(format!("boot image truncated at {o:#x}")))
}

fn u64_at(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| Error::Boot(format!("boot image truncated at {o:#x}")))
}

fn cstr(d: &[u8], o: usize, len: usize) -> Result<String> {
    let s = d.get(o..o + len).ok_or_else(|| Error::Boot("boot image truncated (cmdline)".into()))?;
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    Ok(String::from_utf8_lossy(&s[..end]).into_owned())
}

fn align(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

fn section(d: &[u8], off: usize, len: usize, what: &str) -> Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    d.get(off..off + len)
        .map(|s| s.to_vec())
        .ok_or_else(|| Error::Boot(format!("{what} section [{off:#x}+{len:#x}] exceeds image size {:#x}", d.len())))
}

#[derive(Clone, Debug, Default)]
pub struct BootImage {
    pub header_version: u32,
    pub os_version: u32,
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
    pub cmdline: String,
    /// Only present in v2 boot images.
    pub dtb: Vec<u8>,
}

impl BootImage {
    /// Android version encoded in os_version (e.g. "16.0.0") and patch level.
    pub fn os_version_string(&self) -> String {
        let v = self.os_version >> 11;
        let lvl = self.os_version & 0x7ff;
        format!("{}.{}.{} ({}-{:02})", (v >> 14) & 0x7f, (v >> 7) & 0x7f, v & 0x7f, 2000 + (lvl >> 4), lvl & 0xf)
    }
}

pub fn parse_boot(d: &[u8]) -> Result<BootImage> {
    if d.len() < 1632 || &d[..8] != BOOT_MAGIC {
        return Err(Error::Boot("not an Android boot image (missing ANDROID! magic)".into()));
    }
    let version = u32_at(d, 40)?;
    match version {
        0..=2 => {
            let kernel_size = u32_at(d, 8)? as usize;
            let ramdisk_size = u32_at(d, 16)? as usize;
            let second_size = u32_at(d, 24)? as usize;
            let page = u32_at(d, 36)? as usize;
            if !page.is_power_of_two() || !(2048..=65536).contains(&page) {
                return Err(Error::Boot(format!("invalid boot image page size {page}")));
            }
            let os_version = u32_at(d, 44)?;
            let mut cmdline = cstr(d, 64, 512)?;
            let extra = cstr(d, 608, 1024)?;
            cmdline.push_str(&extra);
            let k_off = page;
            let r_off = k_off + align(kernel_size, page);
            let s_off = r_off + align(ramdisk_size, page);
            let mut next = s_off + align(second_size, page);
            let mut dtb = Vec::new();
            if version >= 1 {
                let rdtbo = u32_at(d, 1632)? as usize;
                next += align(rdtbo, page);
            }
            if version == 2 {
                let dtb_size = u32_at(d, 1648)? as usize;
                dtb = section(d, next, dtb_size, "dtb")?;
            }
            Ok(BootImage {
                header_version: version,
                os_version,
                kernel: section(d, k_off, kernel_size, "kernel")?,
                ramdisk: section(d, r_off, ramdisk_size, "ramdisk")?,
                cmdline,
                dtb,
            })
        }
        3 | 4 => {
            let kernel_size = u32_at(d, 8)? as usize;
            let ramdisk_size = u32_at(d, 12)? as usize;
            let os_version = u32_at(d, 16)?;
            let cmdline = cstr(d, 44, 1536)?;
            let k_off = V3_PAGE;
            let r_off = k_off + align(kernel_size, V3_PAGE);
            Ok(BootImage {
                header_version: version,
                os_version,
                kernel: section(d, k_off, kernel_size, "kernel")?,
                ramdisk: section(d, r_off, ramdisk_size, "ramdisk")?,
                cmdline,
                dtb: Vec::new(),
            })
        }
        v => Err(Error::Boot(format!("unsupported boot image header version {v}"))),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VendorRamdisk {
    pub name: String,
    pub kind: u32,
    pub offset: usize,
    pub size: usize,
}

pub mod ramdisk_type {
    pub const NONE: u32 = 0;
    pub const PLATFORM: u32 = 1;
    pub const RECOVERY: u32 = 2;
    pub const DLKM: u32 = 3;
}

#[derive(Clone, Debug, Default)]
pub struct VendorBootImage {
    pub header_version: u32,
    pub page_size: usize,
    pub cmdline: String,
    pub name: String,
    /// Whole vendor ramdisk section (all fragments back to back).
    pub ramdisk: Vec<u8>,
    pub ramdisks: Vec<VendorRamdisk>,
    pub dtb: Vec<u8>,
    pub bootconfig: Vec<u8>,
}

impl VendorBootImage {
    /// Ramdisk fragments a normal (non-recovery) boot loads, concatenated.
    pub fn normal_boot_ramdisk(&self) -> Vec<u8> {
        if self.ramdisks.is_empty() {
            return self.ramdisk.clone();
        }
        let mut out = Vec::with_capacity(self.ramdisk.len());
        for r in &self.ramdisks {
            if r.kind == ramdisk_type::RECOVERY {
                continue;
            }
            if let Some(s) = self.ramdisk.get(r.offset..r.offset + r.size) {
                out.extend_from_slice(s);
            }
        }
        out
    }
}

pub fn parse_vendor_boot(d: &[u8]) -> Result<VendorBootImage> {
    if d.len() < 2112 || &d[..8] != VENDOR_BOOT_MAGIC {
        return Err(Error::Boot("not a vendor_boot image (missing VNDRBOOT magic)".into()));
    }
    let version = u32_at(d, 8)?;
    if !(3..=4).contains(&version) {
        return Err(Error::Boot(format!("unsupported vendor_boot header version {version}")));
    }
    let page = u32_at(d, 12)? as usize;
    if !page.is_power_of_two() || !(2048..=65536).contains(&page) {
        return Err(Error::Boot(format!("invalid vendor_boot page size {page}")));
    }
    let ramdisk_size = u32_at(d, 24)? as usize;
    let cmdline = cstr(d, 28, 2048)?;
    let name = cstr(d, 2080, 16)?;
    let header_size = u32_at(d, 2096)? as usize;
    let dtb_size = u32_at(d, 2100)? as usize;
    let _dtb_addr = u64_at(d, 2104)?;
    let r_off = align(header_size, page);
    let dtb_off = r_off + align(ramdisk_size, page);
    let mut img = VendorBootImage {
        header_version: version,
        page_size: page,
        cmdline,
        name,
        ramdisk: section(d, r_off, ramdisk_size, "vendor ramdisk")?,
        ramdisks: Vec::new(),
        dtb: section(d, dtb_off, dtb_size, "vendor dtb")?,
        bootconfig: Vec::new(),
    };
    if version == 4 {
        let table_size = u32_at(d, 2112)? as usize;
        let entries = u32_at(d, 2116)? as usize;
        let entry_size = u32_at(d, 2120)? as usize;
        let bc_size = u32_at(d, 2124)? as usize;
        let t_off = dtb_off + align(dtb_size, page);
        let bc_off = t_off + align(table_size, page);
        if entries > 0 && entry_size < 44 {
            return Err(Error::Boot("vendor ramdisk table entry too small".into()));
        }
        for i in 0..entries {
            let e = t_off + i * entry_size;
            let size = u32_at(d, e)? as usize;
            let offset = u32_at(d, e + 4)? as usize;
            let kind = u32_at(d, e + 8)?;
            let rname = cstr(d, e + 12, 32)?;
            if offset + size > ramdisk_size {
                return Err(Error::Boot(format!("vendor ramdisk `{rname}` outside ramdisk section")));
            }
            img.ramdisks.push(VendorRamdisk { name: rname, kind, offset, size });
        }
        img.bootconfig = section(d, bc_off, bc_size, "bootconfig")?;
    }
    Ok(img)
}

/// Everything the kernel needs, assembled.
#[derive(Clone, Debug)]
pub struct AndroidBoot {
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
    pub cmdline: String,
    pub dtb_overlay: Vec<u8>,
}

/// Combine boot / init_boot / vendor_boot the way a v4 bootloader does.
pub fn assemble(
    boot: &BootImage,
    init_boot: Option<&BootImage>,
    vendor: Option<&VendorBootImage>,
    bootconfig_params: &[(String, String)],
    extra_cmdline: &str,
) -> Result<AndroidBoot> {
    if boot.kernel.is_empty() {
        return Err(Error::Boot("boot.img contains no kernel".into()));
    }
    let mut initrd = Vec::new();
    if let Some(v) = vendor {
        initrd.extend_from_slice(&v.normal_boot_ramdisk());
    }
    let generic = match init_boot {
        Some(ib) if !ib.ramdisk.is_empty() => &ib.ramdisk,
        _ => &boot.ramdisk,
    };
    initrd.extend_from_slice(generic);

    let mut cmdline = boot.cmdline.trim().to_string();
    if let Some(v) = vendor {
        if !v.cmdline.trim().is_empty() {
            cmdline.push(' ');
            cmdline.push_str(v.cmdline.trim());
        }
    }
    if !extra_cmdline.trim().is_empty() {
        cmdline.push(' ');
        cmdline.push_str(extra_cmdline.trim());
    }

    let uses_bootconfig = vendor.map(|v| v.header_version >= 4).unwrap_or(false) || !bootconfig_params.is_empty();
    if uses_bootconfig {
        let vendor_bc = vendor.map(|v| v.bootconfig.as_slice()).unwrap_or(&[]);
        let text = bootconfig::merge(vendor_bc, bootconfig_params)?;
        bootconfig::append(&mut initrd, &text)?;
        if !cmdline.split_whitespace().any(|w| w == "bootconfig") {
            cmdline.push_str(" bootconfig");
        }
    }
    Ok(AndroidBoot {
        kernel: boot.kernel.clone(),
        initrd,
        cmdline: cmdline.trim().to_string(),
        dtb_overlay: vendor.map(|v| v.dtb.clone()).unwrap_or_default(),
    })
}

/// Encode `os_version` the way mkbootimg does: A.B.C and YYYY-MM.
pub fn os_version(major: u32, minor: u32, patch: u32, year: u32, month: u32) -> u32 {
    ((major & 0x7f) << 25) | ((minor & 0x7f) << 18) | ((patch & 0x7f) << 11) | (((year.saturating_sub(2000)) & 0x7f) << 4) | (month & 0xf)
}

/// Build a boot image with header v4 (kernel + optional generic ramdisk),
/// equivalent to `mkbootimg --header_version 4`.
pub fn build_boot_v4(kernel: &[u8], ramdisk: &[u8], cmdline: &str, os_version: u32) -> Result<Vec<u8>> {
    if cmdline.len() >= 1536 {
        return Err(Error::Boot(format!("cmdline is {} bytes, boot image v4 allows 1535", cmdline.len())));
    }
    let mut d = vec![0u8; V3_PAGE];
    d[..8].copy_from_slice(BOOT_MAGIC);
    d[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
    d[12..16].copy_from_slice(&(ramdisk.len() as u32).to_le_bytes());
    d[16..20].copy_from_slice(&os_version.to_le_bytes());
    d[20..24].copy_from_slice(&1584u32.to_le_bytes()); // header_size (v4)
    d[40..44].copy_from_slice(&4u32.to_le_bytes());
    d[44..44 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
    // signature_size (1580) stays 0: unsigned, as for AVB-less developer boots.
    d.extend_from_slice(kernel);
    d.resize(align(d.len(), V3_PAGE), 0);
    d.extend_from_slice(ramdisk);
    d.resize(align(d.len(), V3_PAGE), 0);
    Ok(d)
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub fn make_boot_v4(kernel: &[u8], ramdisk: &[u8], cmdline: &str) -> Vec<u8> {
        let mut d = vec![0u8; V3_PAGE];
        d[..8].copy_from_slice(BOOT_MAGIC);
        d[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
        d[12..16].copy_from_slice(&(ramdisk.len() as u32).to_le_bytes());
        // Android 16, 2026-08 patch level
        let osv = ((16u32 << 14) << 11) | ((26 << 4) | 8);
        d[16..20].copy_from_slice(&osv.to_le_bytes());
        d[20..24].copy_from_slice(&1584u32.to_le_bytes());
        d[40..44].copy_from_slice(&4u32.to_le_bytes());
        d[44..44 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        d.extend_from_slice(kernel);
        d.resize(align(d.len(), V3_PAGE), 0);
        d.extend_from_slice(ramdisk);
        d.resize(align(d.len(), V3_PAGE), 0);
        d
    }

    pub fn make_vendor_boot_v4(ramdisks: &[(&str, u32, &[u8])], cmdline: &str, bootconfig: &str) -> Vec<u8> {
        let page = 4096;
        let mut d = vec![0u8; page];
        d[..8].copy_from_slice(VENDOR_BOOT_MAGIC);
        d[8..12].copy_from_slice(&4u32.to_le_bytes());
        d[12..16].copy_from_slice(&(page as u32).to_le_bytes());
        let total: usize = ramdisks.iter().map(|r| r.2.len()).sum();
        d[24..28].copy_from_slice(&(total as u32).to_le_bytes());
        d[28..28 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        d[2080..2084].copy_from_slice(b"apex");
        d[2096..2100].copy_from_slice(&2128u32.to_le_bytes());
        let entry_size = 108usize;
        d[2112..2116].copy_from_slice(&((entry_size * ramdisks.len()) as u32).to_le_bytes());
        d[2116..2120].copy_from_slice(&(ramdisks.len() as u32).to_le_bytes());
        d[2120..2124].copy_from_slice(&(entry_size as u32).to_le_bytes());
        d[2124..2128].copy_from_slice(&(bootconfig.len() as u32).to_le_bytes());
        for r in ramdisks {
            d.extend_from_slice(r.2);
        }
        d.resize(align(d.len(), page), 0);
        // no dtb
        let mut off = 0;
        let mut table = Vec::new();
        for r in ramdisks {
            let mut e = vec![0u8; entry_size];
            e[0..4].copy_from_slice(&(r.2.len() as u32).to_le_bytes());
            e[4..8].copy_from_slice(&(off as u32).to_le_bytes());
            e[8..12].copy_from_slice(&r.1.to_le_bytes());
            e[12..12 + r.0.len()].copy_from_slice(r.0.as_bytes());
            off += r.2.len();
            table.extend_from_slice(&e);
        }
        d.extend_from_slice(&table);
        d.resize(align(d.len(), page), 0);
        d.extend_from_slice(bootconfig.as_bytes());
        d.resize(align(d.len(), page), 0);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn boot_v4_roundtrip() {
        let img = make_boot_v4(&[1u8; 5000], &[2u8; 300], "console=hvc0");
        let b = parse_boot(&img).unwrap();
        assert_eq!(b.header_version, 4);
        assert_eq!(b.kernel, vec![1u8; 5000]);
        assert_eq!(b.ramdisk, vec![2u8; 300]);
        assert_eq!(b.cmdline, "console=hvc0");
        assert_eq!(b.os_version_string(), "16.0.0 (2026-08)");
    }

    #[test]
    fn boot_v0_layout() {
        let page = 2048usize;
        let mut d = vec![0u8; page];
        d[..8].copy_from_slice(BOOT_MAGIC);
        d[8..12].copy_from_slice(&3000u32.to_le_bytes());
        d[16..20].copy_from_slice(&100u32.to_le_bytes());
        d[36..40].copy_from_slice(&(page as u32).to_le_bytes());
        d[64..68].copy_from_slice(b"a=1 ");
        d[608..611].copy_from_slice(b"b=2");
        d.extend(std::iter::repeat_n(7u8, 3000));
        d.resize(page * 3, 0);
        d.extend(std::iter::repeat_n(9u8, 100));
        let b = parse_boot(&d).unwrap();
        assert_eq!(b.kernel.len(), 3000);
        assert!(b.kernel.iter().all(|&x| x == 7));
        assert_eq!(b.ramdisk, vec![9u8; 100]);
        assert_eq!(b.cmdline, "a=1 b=2");
    }

    #[test]
    fn vendor_boot_v4_and_assembly() {
        let vb = make_vendor_boot_v4(
            &[
                ("platform", ramdisk_type::PLATFORM, b"VENDOR"),
                ("recovery", ramdisk_type::RECOVERY, b"RECOV"),
                ("dlkm", ramdisk_type::DLKM, b"DLKM"),
            ],
            "androidboot.console=ttyAMA0",
            "androidboot.hardware=apex\nandroidboot.slot_suffix=_a\n",
        );
        let v = parse_vendor_boot(&vb).unwrap();
        assert_eq!(v.ramdisks.len(), 3);
        assert_eq!(v.normal_boot_ramdisk(), b"VENDORDLKM");
        assert_eq!(v.name, "apex");

        let boot = parse_boot(&make_boot_v4(&[1u8; 64], b"GENERIC-OLD", "console=hvc0")).unwrap();
        let init_boot = parse_boot(&make_boot_v4(&[], b"GENERIC", "")).unwrap();
        let a = assemble(
            &boot,
            Some(&init_boot),
            Some(&v),
            &[("androidboot.serialno".into(), "APEX0001".into()), ("androidboot.hardware".into(), "apex_phone".into())],
            "loglevel=4",
        )
        .unwrap();
        assert!(a.initrd.starts_with(b"VENDORDLKMGENERIC"));
        assert_eq!(a.cmdline, "console=hvc0 androidboot.console=ttyAMA0 loglevel=4 bootconfig");
        let (start, text) = bootconfig::find_trailer(&a.initrd).unwrap();
        assert_eq!(start, b"VENDORDLKMGENERIC".len());
        let text = String::from_utf8(text.to_vec()).unwrap();
        assert!(text.contains("androidboot.hardware = \"apex_phone\""), "{text}");
        assert!(text.contains("androidboot.slot_suffix = \"_a\""), "{text}");
        assert!(text.contains("androidboot.serialno = \"APEX0001\""), "{text}");
        assert_eq!(text.matches("androidboot.hardware").count(), 1);
    }

    #[test]
    fn build_boot_v4_roundtrips() {
        let osv = os_version(12, 1, 0, 2022, 7);
        let img = build_boot_v4(&[9u8; 7000], b"RD", "root=/dev/vda2 ro init=/init", osv).unwrap();
        let b = parse_boot(&img).unwrap();
        assert_eq!(b.header_version, 4);
        assert_eq!(b.kernel, vec![9u8; 7000]);
        assert_eq!(b.ramdisk, b"RD");
        assert_eq!(b.cmdline, "root=/dev/vda2 ro init=/init");
        assert_eq!(b.os_version_string(), "12.1.0 (2022-07)");
        assert!(build_boot_v4(&[], &[], &"x".repeat(1536), 0).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_boot(&[0u8; 4096]).is_err());
        assert!(parse_vendor_boot(&[0u8; 4096]).is_err());
        let mut img = make_boot_v4(&[1u8; 64], b"x", "");
        img[8..12].copy_from_slice(&0x10_0000u32.to_le_bytes()); // kernel larger than file
        assert!(parse_boot(&img).is_err());
    }
}
