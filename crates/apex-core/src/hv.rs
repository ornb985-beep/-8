//! Hypervisor abstraction.
//!
//! The vCPU run loop, PSCI, MMIO dispatch and interrupt plumbing are written
//! against these traits so they can be unit tested on any host. The production
//! implementation is `apex-hvf` (Apple Hypervisor.framework).

use crate::error::Result;

/// ARM64 general purpose / special register selector. Discriminants follow
/// Hypervisor.framework's `hv_reg_t` so the HVF backend can pass them through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    X(u8),
    Pc,
    Fpcr,
    Fpsr,
    Cpsr,
}

impl Reg {
    pub fn hvf_id(self) -> u32 {
        match self {
            Reg::X(n) => {
                debug_assert!(n <= 30);
                n as u32
            }
            Reg::Pc => 31,
            Reg::Fpcr => 32,
            Reg::Fpsr => 33,
            Reg::Cpsr => 34,
        }
    }
}

/// Why `VirtualCpu::run` returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcpuExit {
    /// Another thread asked the vCPU to exit (`hv_vcpus_exit`).
    Canceled,
    /// Synchronous exception taken to the hypervisor.
    Exception {
        syndrome: u64,
        virtual_address: u64,
        physical_address: u64,
    },
    /// The guest virtual timer fired (only without the in-kernel vGIC).
    VtimerActivated,
    Unknown,
}

/// A virtual CPU. Hypervisor.framework requires that a vCPU is created, run
/// and destroyed on the same OS thread, hence no `Send` bound.
pub trait VirtualCpu {
    fn index(&self) -> usize;
    fn run(&mut self) -> Result<VcpuExit>;
    fn get_reg(&self, reg: Reg) -> Result<u64>;
    fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()>;
    /// System register access using the `op0:op1:CRn:CRm:op2` packing used by
    /// `hv_sys_reg_t` (see `apex_arm64::sysreg`).
    fn get_sys_reg(&self, reg: u16) -> Result<u64>;
    fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<()>;
    /// Assert/deassert the vCPU IRQ line (userspace GIC mode only).
    fn set_irq_line(&mut self, asserted: bool) -> Result<()>;
    fn set_vtimer_mask(&mut self, masked: bool) -> Result<()>;
    /// Offset subtracted from the host counter to form CNTVCT_EL0.
    fn vtimer_offset(&self) -> Result<u64>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicMode {
    /// Apple's in-kernel vGICv3 (macOS 15+): interrupts are injected by the
    /// host kernel directly, timer PPIs never exit to userspace.
    Hardware,
    /// GICv3 modelled in userspace (`apex_arm64::gic`), vCPU IRQ line driven
    /// through `hv_vcpu_set_pending_interrupt`.
    Emulated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicRequest {
    Auto,
    Hardware,
    Emulated,
}

#[derive(Clone, Copy, Debug)]
pub struct HvConfig {
    pub ipa_bits: Option<u32>,
    pub gic: GicRequest,
    pub gic_dist_base: u64,
    pub gic_redist_base: u64,
    pub max_vcpus: usize,
}

/// Geometry reported by the interrupt controller implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicGeometry {
    pub dist_size: u64,
    /// Size of one redistributor (RD_base + SGI_base frames).
    pub redist_stride: u64,
    pub spi_base: u32,
    pub spi_count: u32,
    pub vtimer_ppi: u32,
    pub ptimer_ppi: u32,
    pub pmu_ppi: u32,
}

impl Default for GicGeometry {
    fn default() -> Self {
        GicGeometry {
            dist_size: 0x1_0000,
            redist_stride: 0x2_0000,
            spi_base: 32,
            spi_count: 256,
            vtimer_ppi: 27,
            ptimer_ppi: 30,
            pmu_ppi: 23,
        }
    }
}

bitflags_lite! {
    pub struct MemFlags: u64 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC = 1 << 2;
    }
}

/// The VM-wide half of the hypervisor.
pub trait Hypervisor: Send + Sync {
    fn name(&self) -> &str;
    /// Map host memory at guest physical address `gpa`.
    ///
    /// # Safety
    /// `host` must stay valid until unmapped or the VM is destroyed.
    unsafe fn map_memory(&self, host: *mut u8, gpa: u64, size: u64, flags: MemFlags) -> Result<()>;
    fn unmap_memory(&self, gpa: u64, size: u64) -> Result<()>;
    /// Create vCPU `index` on the *calling* thread.
    fn create_vcpu(&self, index: usize, mpidr: u64) -> Result<Box<dyn VirtualCpu>>;
    /// Force the given vCPUs out of guest execution (thread-safe).
    fn kick_vcpus(&self, indices: &[usize]);
    fn gic_mode(&self) -> GicMode;
    fn gic_geometry(&self) -> GicGeometry;
    /// Set an SPI level on the in-kernel vGIC (Hardware mode only).
    fn hw_set_spi(&self, intid: u32, level: bool) -> Result<()>;
    /// Guest generic timer frequency (CNTFRQ_EL0).
    fn counter_frequency(&self) -> u64;
    /// Current host counter in the same units as CNTVCT_EL0 + offset.
    fn host_counter(&self) -> u64;
}

impl MemFlags {
    pub const RWX: MemFlags = MemFlags(7);
    pub const RW: MemFlags = MemFlags(3);
}
