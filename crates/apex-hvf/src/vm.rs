//! VM-wide state: stage-2 memory map, vGIC and vCPU registry.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use apex_core::hv::{GicGeometry, GicMode, GicRequest, HvConfig, Hypervisor, MemFlags, VirtualCpu};
use apex_core::{sys, Error, Result};

use crate::ffi::{self, check};
use crate::vcpu::HvfVcpu;

const NO_VCPU: u64 = u64::MAX;

pub struct HvfVm {
    gic_mode: GicMode,
    geometry: GicGeometry,
    handles: Vec<AtomicU64>,
    vtimer_offset: u64,
    cntfrq: u64,
    mappings: Mutex<Vec<(u64, u64)>>,
}

/// Only one VM per process.
static LIVE: Mutex<bool> = Mutex::new(false);

fn read_cntfrq() -> u64 {
    let f: u64;
    // SAFETY: CNTFRQ_EL0 is readable from EL0 on Apple Silicon.
    unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    if f == 0 {
        24_000_000
    } else {
        f
    }
}

pub fn capabilities() -> String {
    let late = ffi::late();
    let mut max_vcpus = 0u32;
    // SAFETY: out-pointer valid.
    unsafe { ffi::hv_vm_get_max_vcpu_count(&mut max_vcpus) };
    let mut max_ipa = 0u32;
    if let Some(f) = late.vm_config_get_max_ipa_size {
        // SAFETY: out-pointer valid.
        unsafe { f(&mut max_ipa) };
    }
    let ipa = if max_ipa == 0 { "unknown".to_string() } else { format!("{max_ipa} bits") };
    format!(
        "Hypervisor.framework: max vCPUs {max_vcpus}, max IPA {ipa}, in-kernel vGICv3 {}, CNTFRQ {} Hz",
        if late.has_gic() { "available (macOS 15+)" } else { "unavailable (userspace GICv3 will be used)" },
        read_cntfrq()
    )
}

impl HvfVm {
    pub fn create(cfg: &HvConfig) -> Result<Arc<HvfVm>> {
        let mut live = LIVE.lock().unwrap();
        if *live {
            return Err(Error::Hypervisor("a VM already exists in this process".into()));
        }
        let late = ffi::late();

        // VM configuration (IPA size) when the API exists (macOS 13+).
        let mut config: *mut std::ffi::c_void = std::ptr::null_mut();
        if let (Some(create), Some(set_ipa)) = (late.vm_config_create, late.vm_config_set_ipa_size) {
            // SAFETY: returns a retained OS object or NULL.
            config = unsafe { create() };
            if !config.is_null() {
                if let Some(bits) = cfg.ipa_bits {
                    let mut default_bits = 0u32;
                    if let Some(get_default) = late.vm_config_get_default_ipa_size {
                        // SAFETY: out-pointer valid.
                        unsafe { get_default(&mut default_bits) };
                    }
                    if default_bits < bits {
                        // SAFETY: valid config object.
                        let r = unsafe { set_ipa(config, bits) };
                        if r != ffi::HV_SUCCESS {
                            // Not fatal: hv_vm_create reports the real problem
                            // (e.g. no hypervisor access) or works with the default.
                            apex_core::warn!("hv_vm_config_set_ipa_size({bits}): {}", ffi::err_name(r));
                        }
                    }
                }
            }
        }
        // SAFETY: config is NULL or a valid config object.
        let r = unsafe { ffi::hv_vm_create(config) };
        if !config.is_null() {
            // SAFETY: we own one reference.
            unsafe { ffi::os_release(config) };
        }
        check(r, "hv_vm_create")?;

        let want_hw = match cfg.gic {
            GicRequest::Auto => late.has_gic(),
            GicRequest::Hardware => {
                if !late.has_gic() {
                    // SAFETY: VM was created above.
                    unsafe { ffi::hv_vm_destroy() };
                    return Err(Error::Unsupported("in-kernel vGIC requires macOS 15 or newer".into()));
                }
                true
            }
            GicRequest::Emulated => false,
        };

        let mut geometry = GicGeometry::default();
        let gic_mode = if want_hw {
            match Self::create_gic(cfg, &mut geometry) {
                Ok(()) => GicMode::Hardware,
                Err(e) => {
                    // SAFETY: VM was created above.
                    unsafe { ffi::hv_vm_destroy() };
                    return Err(e);
                }
            }
        } else {
            GicMode::Emulated
        };

        *live = true;
        let vm = Arc::new(HvfVm {
            gic_mode,
            geometry,
            handles: (0..cfg.max_vcpus).map(|_| AtomicU64::new(NO_VCPU)).collect(),
            vtimer_offset: sys::host_ticks(),
            cntfrq: read_cntfrq(),
            mappings: Mutex::new(Vec::new()),
        });
        apex_core::info!("{}; using {:?} GIC", capabilities(), gic_mode);
        Ok(vm)
    }

    fn create_gic(cfg: &HvConfig, geo: &mut GicGeometry) -> Result<()> {
        let late = ffi::late();
        // SAFETY: all function pointers were resolved (has_gic) and are
        // called with valid arguments, before any vCPU exists.
        unsafe {
            let gcfg = (late.gic_config_create.unwrap())();
            if gcfg.is_null() {
                return Err(Error::Hypervisor("hv_gic_config_create failed".into()));
            }
            check((late.gic_config_set_distributor_base.unwrap())(gcfg, cfg.gic_dist_base), "hv_gic_config_set_distributor_base")?;
            check((late.gic_config_set_redistributor_base.unwrap())(gcfg, cfg.gic_redist_base), "hv_gic_config_set_redistributor_base")?;
            let r = (late.gic_create.unwrap())(gcfg);
            ffi::os_release(gcfg);
            check(r, "hv_gic_create")?;

            let mut sz = 0usize;
            if let Some(f) = late.gic_get_distributor_size {
                if f(&mut sz) == ffi::HV_SUCCESS && sz > 0 {
                    geo.dist_size = sz as u64;
                }
            }
            if let Some(f) = late.gic_get_redistributor_size {
                if f(&mut sz) == ffi::HV_SUCCESS && sz > 0 {
                    geo.redist_stride = sz as u64;
                }
            }
            if let Some(f) = late.gic_get_spi_interrupt_range {
                let (mut base, mut count) = (0u32, 0u32);
                if f(&mut base, &mut count) == ffi::HV_SUCCESS && count > 0 {
                    geo.spi_base = base;
                    geo.spi_count = count;
                }
            }
            if let Some(f) = late.gic_get_intid {
                let mut v = 0u32;
                if f(ffi::HV_GIC_INT_EL1_VIRTUAL_TIMER, &mut v) == ffi::HV_SUCCESS {
                    geo.vtimer_ppi = v;
                }
                if f(ffi::HV_GIC_INT_EL1_PHYSICAL_TIMER, &mut v) == ffi::HV_SUCCESS {
                    geo.ptimer_ppi = v;
                }
                if f(ffi::HV_GIC_INT_PERFORMANCE_MONITOR, &mut v) == ffi::HV_SUCCESS {
                    geo.pmu_ppi = v;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn register_vcpu(&self, index: usize, handle: u64) {
        if let Some(h) = self.handles.get(index) {
            h.store(handle, Ordering::Release);
        }
    }

    pub(crate) fn unregister_vcpu(&self, index: usize) {
        if let Some(h) = self.handles.get(index) {
            h.store(NO_VCPU, Ordering::Release);
        }
    }
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        for (ipa, size) in self.mappings.lock().unwrap().drain(..) {
            // SAFETY: mapped by us.
            unsafe { ffi::hv_vm_unmap(ipa, size as usize) };
        }
        // SAFETY: all vCPUs were destroyed by their threads before the last
        // Arc<HvfVm> reference went away.
        let r = unsafe { ffi::hv_vm_destroy() };
        if r != ffi::HV_SUCCESS {
            apex_core::warn!("hv_vm_destroy: {}", ffi::err_name(r));
        }
        *LIVE.lock().unwrap() = false;
    }
}

impl Hypervisor for HvfVm {
    fn name(&self) -> &str {
        "hvf"
    }

    unsafe fn map_memory(&self, host: *mut u8, gpa: u64, size: u64, flags: MemFlags) -> Result<()> {
        check(ffi::hv_vm_map(host as *mut _, gpa, size as usize, flags.bits()), "hv_vm_map")?;
        self.mappings.lock().unwrap().push((gpa, size));
        Ok(())
    }

    fn unmap_memory(&self, gpa: u64, size: u64) -> Result<()> {
        // SAFETY: plain unmap of an IPA range.
        check(unsafe { ffi::hv_vm_unmap(gpa, size as usize) }, "hv_vm_unmap")?;
        self.mappings.lock().unwrap().retain(|&(g, s)| !(g == gpa && s == size));
        Ok(())
    }

    fn create_vcpu(&self, index: usize, mpidr: u64) -> Result<Box<dyn VirtualCpu>> {
        let v = HvfVcpu::create(index, mpidr, self.vtimer_offset)?;
        self.register_vcpu(index, v.handle());
        Ok(Box::new(VcpuGuard { vcpu: v, vm: self as *const HvfVm }))
    }

    fn kick_vcpus(&self, indices: &[usize]) {
        let hs: Vec<u64> =
            indices.iter().filter_map(|&i| self.handles.get(i).map(|h| h.load(Ordering::Acquire))).filter(|&h| h != NO_VCPU).collect();
        if !hs.is_empty() {
            // SAFETY: handles of live vCPUs (thread-safe API).
            unsafe { ffi::hv_vcpus_exit(hs.as_ptr(), hs.len() as u32) };
        }
    }

    fn gic_mode(&self) -> GicMode {
        self.gic_mode
    }

    fn gic_geometry(&self) -> GicGeometry {
        self.geometry
    }

    fn hw_set_spi(&self, intid: u32, level: bool) -> Result<()> {
        let f = ffi::late().gic_set_spi.ok_or_else(|| Error::Unsupported("hv_gic_set_spi".into()))?;
        // SAFETY: thread-safe API, valid after hv_gic_create.
        check(unsafe { f(intid, level) }, "hv_gic_set_spi")
    }

    fn counter_frequency(&self) -> u64 {
        self.cntfrq
    }

    fn host_counter(&self) -> u64 {
        sys::host_ticks()
    }
}

/// Unregisters the vCPU handle from the kick table when dropped.
struct VcpuGuard {
    vcpu: HvfVcpu,
    vm: *const HvfVm,
}

impl Drop for VcpuGuard {
    fn drop(&mut self) {
        // SAFETY: the VM outlives its vCPUs (vCPU threads are joined before
        // the machine drops its Arc<dyn Hypervisor>).
        unsafe { (*self.vm).unregister_vcpu(self.vcpu.index()) };
    }
}

impl VirtualCpu for VcpuGuard {
    fn index(&self) -> usize {
        self.vcpu.index()
    }
    fn run(&mut self) -> Result<apex_core::hv::VcpuExit> {
        self.vcpu.run()
    }
    fn get_reg(&self, reg: apex_core::hv::Reg) -> Result<u64> {
        self.vcpu.get_reg(reg)
    }
    fn set_reg(&mut self, reg: apex_core::hv::Reg, value: u64) -> Result<()> {
        self.vcpu.set_reg(reg, value)
    }
    fn get_sys_reg(&self, reg: u16) -> Result<u64> {
        self.vcpu.get_sys_reg(reg)
    }
    fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<()> {
        self.vcpu.set_sys_reg(reg, value)
    }
    fn set_irq_line(&mut self, asserted: bool) -> Result<()> {
        self.vcpu.set_irq_line(asserted)
    }
    fn set_vtimer_mask(&mut self, masked: bool) -> Result<()> {
        self.vcpu.set_vtimer_mask(masked)
    }
    fn vtimer_offset(&self) -> Result<u64> {
        self.vcpu.vtimer_offset()
    }
}
