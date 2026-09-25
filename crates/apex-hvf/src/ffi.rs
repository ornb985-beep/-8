//! Raw Hypervisor.framework bindings (arm64).
//!
//! The macOS 11 base API is linked directly. APIs added later (VM config in
//! macOS 13, the vGIC in macOS 15) are resolved with `dlsym` at runtime so a
//! single binary runs on every supported macOS and simply falls back to the
//! userspace GIC where the in-kernel one does not exist.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_void, CString};
use std::sync::OnceLock;

pub type hv_return_t = i32;
pub type hv_vcpu_t = u64;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;

pub const HV_SUCCESS: hv_return_t = 0;
pub const HV_ERROR: hv_return_t = 0xfae9_4001u32 as i32;
pub const HV_BUSY: hv_return_t = 0xfae9_4002u32 as i32;
pub const HV_BAD_ARGUMENT: hv_return_t = 0xfae9_4003u32 as i32;
pub const HV_ILLEGAL_GUEST_STATE: hv_return_t = 0xfae9_4004u32 as i32;
pub const HV_NO_RESOURCES: hv_return_t = 0xfae9_4005u32 as i32;
pub const HV_NO_DEVICE: hv_return_t = 0xfae9_4006u32 as i32;
pub const HV_DENIED: hv_return_t = 0xfae9_4007u32 as i32;
pub const HV_UNSUPPORTED: hv_return_t = 0xfae9_400fu32 as i32;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;
pub const HV_INTERRUPT_TYPE_FIQ: u32 = 1;

/// hv_gic_intid_t
pub const HV_GIC_INT_PERFORMANCE_MONITOR: u32 = 23;
pub const HV_GIC_INT_MAINTENANCE: u32 = 25;
pub const HV_GIC_INT_EL2_PHYSICAL_TIMER: u32 = 26;
pub const HV_GIC_INT_EL1_VIRTUAL_TIMER: u32 = 27;
pub const HV_GIC_INT_EL1_PHYSICAL_TIMER: u32 = 30;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    pub fn hv_vm_create(config: *mut c_void) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(addr: *mut c_void, ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_get_max_vcpu_count(max: *mut u32) -> hv_return_t;

    pub fn hv_vcpu_create(vcpu: *mut hv_vcpu_t, exit: *mut *const hv_vcpu_exit_t, config: *mut c_void) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, count: u32) -> hv_return_t;
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;
    pub fn hv_vcpu_set_pending_interrupt(vcpu: hv_vcpu_t, kind: u32, pending: bool) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;
    pub fn hv_vcpu_set_trap_debug_exceptions(vcpu: hv_vcpu_t, value: bool) -> hv_return_t;
}

extern "C" {
    pub fn os_release(object: *mut c_void);
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// Late-bound entry points (macOS 13+/15+).
pub struct Late {
    pub vm_config_create: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub vm_config_set_ipa_size: Option<unsafe extern "C" fn(*mut c_void, u32) -> hv_return_t>,
    pub vm_config_get_max_ipa_size: Option<unsafe extern "C" fn(*mut u32) -> hv_return_t>,

    pub gic_config_create: Option<unsafe extern "C" fn() -> *mut c_void>,
    pub gic_config_set_distributor_base: Option<unsafe extern "C" fn(*mut c_void, hv_ipa_t) -> hv_return_t>,
    pub gic_config_set_redistributor_base: Option<unsafe extern "C" fn(*mut c_void, hv_ipa_t) -> hv_return_t>,
    pub gic_create: Option<unsafe extern "C" fn(*mut c_void) -> hv_return_t>,
    pub gic_set_spi: Option<unsafe extern "C" fn(u32, bool) -> hv_return_t>,
    pub gic_get_distributor_size: Option<unsafe extern "C" fn(*mut usize) -> hv_return_t>,
    pub gic_get_redistributor_size: Option<unsafe extern "C" fn(*mut usize) -> hv_return_t>,
    pub gic_get_redistributor_region_size: Option<unsafe extern "C" fn(*mut usize) -> hv_return_t>,
    pub gic_get_spi_interrupt_range: Option<unsafe extern "C" fn(*mut u32, *mut u32) -> hv_return_t>,
    pub gic_get_intid: Option<unsafe extern "C" fn(u32, *mut u32) -> hv_return_t>,
    pub gic_reset: Option<unsafe extern "C" fn() -> hv_return_t>,
}

fn sym<T: Copy>(name: &str) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    let c = CString::new(name).ok()?;
    // SAFETY: looking up a symbol in the already loaded images.
    let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    if p.is_null() {
        None
    } else {
        // SAFETY: T is a function pointer type matching the symbol's C signature.
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }
}

pub fn late() -> &'static Late {
    static L: OnceLock<Late> = OnceLock::new();
    L.get_or_init(|| Late {
        vm_config_create: sym("hv_vm_config_create"),
        vm_config_set_ipa_size: sym("hv_vm_config_set_ipa_size"),
        vm_config_get_max_ipa_size: sym("hv_vm_config_get_max_ipa_size"),
        gic_config_create: sym("hv_gic_config_create"),
        gic_config_set_distributor_base: sym("hv_gic_config_set_distributor_base"),
        gic_config_set_redistributor_base: sym("hv_gic_config_set_redistributor_base"),
        gic_create: sym("hv_gic_create"),
        gic_set_spi: sym("hv_gic_set_spi"),
        gic_get_distributor_size: sym("hv_gic_get_distributor_size"),
        gic_get_redistributor_size: sym("hv_gic_get_redistributor_size"),
        gic_get_redistributor_region_size: sym("hv_gic_get_redistributor_region_size"),
        gic_get_spi_interrupt_range: sym("hv_gic_get_spi_interrupt_range"),
        gic_get_intid: sym("hv_gic_get_intid"),
        gic_reset: sym("hv_gic_reset"),
    })
}

impl Late {
    pub fn has_gic(&self) -> bool {
        self.gic_config_create.is_some()
            && self.gic_config_set_distributor_base.is_some()
            && self.gic_config_set_redistributor_base.is_some()
            && self.gic_create.is_some()
            && self.gic_set_spi.is_some()
    }
}

pub fn err_name(r: hv_return_t) -> &'static str {
    match r {
        HV_SUCCESS => "HV_SUCCESS",
        HV_ERROR => "HV_ERROR",
        HV_BUSY => "HV_BUSY",
        HV_BAD_ARGUMENT => "HV_BAD_ARGUMENT",
        HV_ILLEGAL_GUEST_STATE => "HV_ILLEGAL_GUEST_STATE",
        HV_NO_RESOURCES => "HV_NO_RESOURCES",
        HV_NO_DEVICE => "HV_NO_DEVICE",
        HV_DENIED => "HV_DENIED (missing com.apple.security.hypervisor entitlement?)",
        HV_UNSUPPORTED => "HV_UNSUPPORTED",
        _ => "unknown hv_return_t",
    }
}

pub fn check(r: hv_return_t, what: &str) -> apex_core::Result<()> {
    if r == HV_SUCCESS {
        Ok(())
    } else {
        Err(apex_core::Error::Hypervisor(format!("{what}: {} ({r:#x})", err_name(r))))
    }
}
