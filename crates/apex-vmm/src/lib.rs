//! Apex-AOSP virtual machine monitor.
//!
//! ```text
//!  ┌───────────── macOS app (Swift, AppKit, Metal @ 120 Hz) ─────────────┐
//!  │  CAMetalDisplayLink ─ acquire frame ─ newBufferWithBytesNoCopy       │
//!  │  NSEvent ─ multitouch / keys                    C ABI (include/apex.h)
//!  └────────────────────────────────┬─────────────────────────────────────┘
//!  ┌──────────────── apex-vmm (this crate) ───────────────────────────────┐
//!  │ Machine: RAM, GICv3, PL011/PL031/battery, virtio-mmio ×N, DT, boot   │
//!  │ vCPU threads: hv_vcpu_run ⇄ MMIO / sysreg / WFI / PSCI               │
//!  └────────────────────────────────┬─────────────────────────────────────┘
//!                  Hypervisor.framework (EL2, stage-2, vGICv3)
//! ```

pub mod config;
pub mod fdt_gen;
pub mod ffi;
pub mod inspect;
pub mod machine;
pub mod selftest;
pub mod vcpu;

pub use config::VmConfig;
pub use machine::{Controls, HostHooks, Machine};
pub use vcpu::StopReason;
