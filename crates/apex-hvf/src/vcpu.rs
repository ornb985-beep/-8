//! One Hypervisor.framework vCPU. Must live and die on a single thread.

use apex_core::hv::{Reg, VcpuExit};
use apex_core::{Error, Result};

use crate::ffi::{self, check, hv_vcpu_exit_t, hv_vcpu_t};

pub struct HvfVcpu {
    index: usize,
    handle: hv_vcpu_t,
    exit: *const hv_vcpu_exit_t,
    // HVF vCPUs are bound to their creating thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl HvfVcpu {
    pub fn create(index: usize, mpidr: u64, vtimer_offset: u64) -> Result<HvfVcpu> {
        let mut handle: hv_vcpu_t = 0;
        let mut exit: *const hv_vcpu_exit_t = std::ptr::null();
        // SAFETY: out-pointers valid; NULL config = defaults.
        check(unsafe { ffi::hv_vcpu_create(&mut handle, &mut exit, std::ptr::null_mut()) }, "hv_vcpu_create")?;
        if exit.is_null() {
            return Err(Error::Hypervisor("hv_vcpu_create returned no exit structure".into()));
        }
        let v = HvfVcpu { index, handle, exit, _not_send: std::marker::PhantomData };
        // SAFETY: handle is a live vCPU owned by this thread.
        unsafe {
            check(ffi::hv_vcpu_set_sys_reg(handle, apex_arm64::sysreg::MPIDR_EL1.0, mpidr), "set MPIDR_EL1")?;
            // All vCPUs share one virtual counter base.
            check(ffi::hv_vcpu_set_vtimer_offset(handle, vtimer_offset), "hv_vcpu_set_vtimer_offset")?;
            ffi::hv_vcpu_set_trap_debug_exceptions(handle, false);
        }
        Ok(v)
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn handle(&self) -> hv_vcpu_t {
        self.handle
    }

    pub fn run(&mut self) -> Result<VcpuExit> {
        // SAFETY: running our own vCPU on its thread.
        check(unsafe { ffi::hv_vcpu_run(self.handle) }, "hv_vcpu_run")?;
        // SAFETY: the exit structure is valid for the vCPU's lifetime and
        // only written by the framework during hv_vcpu_run.
        let e = unsafe { *self.exit };
        Ok(match e.reason {
            ffi::HV_EXIT_REASON_CANCELED => VcpuExit::Canceled,
            ffi::HV_EXIT_REASON_EXCEPTION => VcpuExit::Exception {
                syndrome: e.exception.syndrome,
                virtual_address: e.exception.virtual_address,
                physical_address: e.exception.physical_address,
            },
            ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => VcpuExit::VtimerActivated,
            _ => VcpuExit::Unknown,
        })
    }

    pub fn get_reg(&self, reg: Reg) -> Result<u64> {
        let mut v = 0u64;
        // SAFETY: valid out-pointer.
        check(unsafe { ffi::hv_vcpu_get_reg(self.handle, reg.hvf_id(), &mut v) }, "hv_vcpu_get_reg")?;
        Ok(v)
    }

    pub fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
        // SAFETY: plain call.
        check(unsafe { ffi::hv_vcpu_set_reg(self.handle, reg.hvf_id(), value) }, "hv_vcpu_set_reg")
    }

    pub fn get_sys_reg(&self, reg: u16) -> Result<u64> {
        let mut v = 0u64;
        // SAFETY: valid out-pointer.
        check(unsafe { ffi::hv_vcpu_get_sys_reg(self.handle, reg, &mut v) }, "hv_vcpu_get_sys_reg")?;
        Ok(v)
    }

    pub fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<()> {
        // SAFETY: plain call.
        check(unsafe { ffi::hv_vcpu_set_sys_reg(self.handle, reg, value) }, "hv_vcpu_set_sys_reg")
    }

    pub fn set_irq_line(&mut self, asserted: bool) -> Result<()> {
        // SAFETY: plain call.
        check(
            unsafe { ffi::hv_vcpu_set_pending_interrupt(self.handle, ffi::HV_INTERRUPT_TYPE_IRQ, asserted) },
            "hv_vcpu_set_pending_interrupt",
        )
    }

    pub fn set_vtimer_mask(&mut self, masked: bool) -> Result<()> {
        // SAFETY: plain call.
        check(unsafe { ffi::hv_vcpu_set_vtimer_mask(self.handle, masked) }, "hv_vcpu_set_vtimer_mask")
    }

    pub fn vtimer_offset(&self) -> Result<u64> {
        let mut v = 0u64;
        // SAFETY: valid out-pointer.
        check(unsafe { ffi::hv_vcpu_get_vtimer_offset(self.handle, &mut v) }, "hv_vcpu_get_vtimer_offset")?;
        Ok(v)
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: destroying our own vCPU on its thread.
        let r = unsafe { ffi::hv_vcpu_destroy(self.handle) };
        if r != ffi::HV_SUCCESS {
            apex_core::warn!("hv_vcpu_destroy({}): {}", self.index, ffi::err_name(r));
        }
    }
}
