//! Direct kernel boot: the VMM places the kernel, initrd and DTB in guest RAM
//! and starts the boot CPU at the kernel entry with x0 = DTB address.

pub mod android;
pub mod bootconfig;
pub mod image;
pub mod inflate;
pub mod lz4;

use apex_core::mem::{GuestAddress, GuestMemory};
use apex_core::{align_down, align_up, Error, Result, MIB};

use image::ImageHeader;

/// Maximum DTB size accepted by the arm64 boot protocol.
pub const DTB_MAX: u64 = 2 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub kernel: GuestAddress,
    pub kernel_end: GuestAddress,
    pub initrd: Option<(GuestAddress, u64)>,
    pub dtb: GuestAddress,
}

/// Decide where kernel, initrd and DTB go inside `[ram_base, ram_base+ram_size)`.
pub fn plan(ram_base: u64, ram_size: u64, hdr: &ImageHeader, kernel_len: usize, initrd_len: usize) -> Result<Placement> {
    let ram_end = ram_base + ram_size;
    let kernel = align_up(ram_base, 2 * MIB) + hdr.text_offset;
    // Leave some headroom above the image for early page tables the kernel
    // may allocate right after _end.
    let kernel_end = align_up(kernel + hdr.footprint(kernel_len), 2 * MIB);
    let dtb = align_down(ram_end - DTB_MAX, 2 * MIB);
    if kernel_end > dtb {
        return Err(Error::Boot(format!("kernel ({} MiB) does not fit in guest RAM", hdr.footprint(kernel_len) / MIB)));
    }
    let initrd = if initrd_len > 0 {
        let len = initrd_len as u64;
        let start = align_down(dtb.checked_sub(len).ok_or_else(|| Error::Boot("initrd too large".into()))?, 64 * 1024);
        if start < kernel_end {
            return Err(Error::Boot(format!("initrd ({} MiB) does not fit in guest RAM", len / MIB)));
        }
        Some((GuestAddress(start), len))
    } else {
        None
    };
    Ok(Placement { kernel: GuestAddress(kernel), kernel_end: GuestAddress(kernel_end), initrd, dtb: GuestAddress(dtb) })
}

/// Copy the kernel and initrd into guest memory.
pub fn load(mem: &GuestMemory, p: &Placement, kernel: &[u8], initrd: &[u8]) -> Result<()> {
    mem.write(kernel, p.kernel)?;
    if let Some((addr, len)) = p.initrd {
        debug_assert_eq!(len as usize, initrd.len());
        mem.write(initrd, addr)?;
    }
    Ok(())
}

pub fn write_dtb(mem: &GuestMemory, p: &Placement, dtb: &[u8]) -> Result<()> {
    if dtb.len() as u64 > DTB_MAX {
        return Err(Error::Boot(format!("device tree is {} bytes (max {})", dtb.len(), DTB_MAX)));
    }
    mem.write(dtb, p.dtb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::GIB;

    #[test]
    fn placement_is_disjoint_and_aligned() {
        let img = image::fake_image(64 * 1024, 0, 40 * MIB);
        let (_, hdr) = image::prepare_kernel(&img).unwrap();
        let p = plan(0x8000_0000, 4 * GIB, &hdr, img.len(), 20 * MIB as usize).unwrap();
        assert_eq!(p.kernel.0, 0x8000_0000);
        assert_eq!(p.kernel.0 % (2 * MIB), 0);
        let (ia, il) = p.initrd.unwrap();
        assert!(ia.0 >= p.kernel_end.0);
        assert!(ia.0 + il <= p.dtb.0);
        assert_eq!(p.dtb.0 % (2 * MIB), 0);
        assert!(p.dtb.0 + DTB_MAX <= 0x8000_0000 + 4 * GIB);
    }

    #[test]
    fn too_small_ram_fails() {
        let img = image::fake_image(4096, 0, 300 * MIB);
        let (_, hdr) = image::prepare_kernel(&img).unwrap();
        assert!(plan(0x8000_0000, 256 * MIB, &hdr, img.len(), 0).is_err());
    }

    #[test]
    fn load_writes_bytes() {
        let page = apex_core::sys::host_page_size() as u64;
        let mem = GuestMemory::new(&[(GuestAddress(0x8000_0000), align_up(64 * MIB, page))]).unwrap();
        let img = image::fake_image(8192, 0, 4 * MIB);
        let (raw, hdr) = image::prepare_kernel(&img).unwrap();
        let p = plan(0x8000_0000, 64 * MIB, &hdr, raw.len(), 1000).unwrap();
        load(&mem, &p, &raw, &[5u8; 1000]).unwrap();
        let mut b = [0u8; 64];
        mem.read(&mut b, p.kernel).unwrap();
        assert_eq!(&b[..], &raw[..64]);
        let (ia, _) = p.initrd.unwrap();
        assert_eq!(mem.read_obj::<u8>(ia).unwrap(), 5);
    }
}
