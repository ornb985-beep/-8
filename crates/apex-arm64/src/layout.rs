//! Guest physical memory map of the `apex-virt` machine.
//!
//! ```text
//! 0x0800_0000  GICv3 distributor            64 KiB
//! 0x080a_0000  GICv3 redistributors         128 KiB per vCPU (up to 123)
//! 0x0900_0000  PL011 UART                   4 KiB   SPI 1
//! 0x0901_0000  PL031 RTC                    4 KiB   SPI 2
//! 0x0902_0000  goldfish battery             4 KiB   SPI 3
//! 0x0a00_0000  virtio-mmio slots            32 x 4 KiB, SPI 16..47
//! 0x8000_0000  RAM                          up to 30 GiB
//! 0x8_0000_0000 host-visible GPU window     8 GiB (virtio-gpu shm id 1)
//! ```
//! Everything fits a 36-bit IPA space, which every Apple Silicon generation
//! supports.

use apex_core::{Error, Result, GIB, MIB};

pub const GIC_DIST_BASE: u64 = 0x0800_0000;
pub const GIC_DIST_SIZE: u64 = 0x1_0000;
pub const GIC_REDIST_BASE: u64 = 0x080a_0000;
pub const GIC_REDIST_LIMIT: u64 = 0x0900_0000;
pub const GIC_REDIST_STRIDE: u64 = 0x2_0000;

pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x1000;
pub const UART_SPI: u32 = 1;

pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SIZE: u64 = 0x1000;
pub const RTC_SPI: u32 = 2;

pub const BATTERY_BASE: u64 = 0x0902_0000;
pub const BATTERY_SIZE: u64 = 0x1000;
pub const BATTERY_SPI: u32 = 3;

pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_MMIO_STRIDE: u64 = 0x1000;
pub const VIRTIO_MMIO_SLOTS: u32 = 32;
pub const VIRTIO_SPI_BASE: u32 = 16;

pub const RAM_BASE: u64 = 0x8000_0000;
pub const HOSTMEM_BASE: u64 = 0x8_0000_0000;
pub const HOSTMEM_SIZE: u64 = 8 * GIB;
pub const MAX_RAM: u64 = HOSTMEM_BASE - RAM_BASE;
pub const IPA_BITS: u32 = 36;

const _: () = assert!(HOSTMEM_BASE + HOSTMEM_SIZE <= 1u64 << IPA_BITS);

/// Maximum vCPUs the redistributor window can describe.
pub const fn max_vcpus(redist_stride: u64) -> usize {
    ((GIC_REDIST_LIMIT - GIC_REDIST_BASE) / redist_stride) as usize
}

pub fn virtio_slot(i: u32) -> (u64, u32) {
    (VIRTIO_MMIO_BASE + i as u64 * VIRTIO_MMIO_STRIDE, VIRTIO_SPI_BASE + i)
}

pub fn validate_ram(size: u64) -> Result<()> {
    if size < 256 * MIB {
        return Err(Error::Config("guest RAM must be at least 256 MiB".into()));
    }
    if size > MAX_RAM {
        return Err(Error::Config(format!("guest RAM is limited to {} GiB", MAX_RAM / GIB)));
    }
    if size % (2 * MIB) != 0 {
        return Err(Error::Config("guest RAM must be a multiple of 2 MiB".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_is_consistent() {
        assert_eq!(max_vcpus(GIC_REDIST_STRIDE), 123);
        let (last, spi) = virtio_slot(VIRTIO_MMIO_SLOTS - 1);
        assert!(last + VIRTIO_MMIO_STRIDE <= RAM_BASE);
        assert_eq!(spi, 47);
        assert!(validate_ram(8 * GIB).is_ok());
        assert!(validate_ram(31 * GIB).is_err());
        assert!(validate_ram(100 * MIB).is_err());
    }
}
