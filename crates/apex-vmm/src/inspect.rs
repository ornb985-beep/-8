//! A hypervisor that never runs guest code. Lets `apex inspect` assemble a
//! machine (memory map, device tree, boot payload) on any host, including
//! Linux CI, to validate profiles and images before booting on a Mac.

use apex_core::hv::{GicGeometry, GicMode, Hypervisor, MemFlags, VirtualCpu};
use apex_core::{Error, Result};

pub struct DryRunHypervisor {
    pub mode: GicMode,
}

impl Hypervisor for DryRunHypervisor {
    fn name(&self) -> &str {
        "dry-run"
    }
    unsafe fn map_memory(&self, _host: *mut u8, _gpa: u64, _size: u64, _flags: MemFlags) -> Result<()> {
        Ok(())
    }
    fn unmap_memory(&self, _gpa: u64, _size: u64) -> Result<()> {
        Ok(())
    }
    fn create_vcpu(&self, _index: usize, _mpidr: u64) -> Result<Box<dyn VirtualCpu>> {
        Err(Error::Unsupported("dry-run hypervisor cannot execute guest code".into()))
    }
    fn kick_vcpus(&self, _indices: &[usize]) {}
    fn gic_mode(&self) -> GicMode {
        self.mode
    }
    fn gic_geometry(&self) -> GicGeometry {
        GicGeometry::default()
    }
    fn hw_set_spi(&self, _intid: u32, _level: bool) -> Result<()> {
        Ok(())
    }
    fn counter_frequency(&self) -> u64 {
        24_000_000
    }
    fn host_counter(&self) -> u64 {
        apex_core::sys::host_ticks()
    }
}
