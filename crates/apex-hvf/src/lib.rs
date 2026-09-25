//! Apple Hypervisor.framework back end.
//!
//! On macOS/aarch64 this drives real hardware virtualization (EL2 managed by
//! XNU, stage-2 translation, the in-kernel vGICv3 on macOS 15+). On every
//! other target the crate compiles to a stub so the rest of the VMM can be
//! built and unit-tested anywhere.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod ffi;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vcpu;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vm;

use std::sync::Arc;

use apex_core::hv::{HvConfig, Hypervisor};
use apex_core::Result;

/// Create the VM. Only one VM may exist per process (a Hypervisor.framework
/// restriction); drop the returned object before creating another.
pub fn create(cfg: &HvConfig) -> Result<Arc<dyn Hypervisor>> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok(vm::HvfVm::create(cfg)?)
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = cfg;
        Err(apex_core::Error::Unsupported("Hypervisor.framework is only available on Apple Silicon Macs (aarch64-apple-darwin)".into()))
    }
}

/// Human readable description of host virtualization capabilities.
pub fn host_capabilities() -> String {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        vm::capabilities()
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        "Hypervisor.framework: unavailable on this host".into()
    }
}
