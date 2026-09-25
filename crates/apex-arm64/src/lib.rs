//! ARM64 architecture layer for Apex-AOSP.
//!
//! Everything here is pure logic: decoding exception syndromes, the PSCI
//! firmware interface the VMM implements in place of ATF, the Linux/Android
//! boot protocols, and a userspace GICv3 used on hosts without Apple's
//! in-kernel vGIC.

pub mod boot;
pub mod cpu;
pub mod esr;
pub mod gic;
pub mod layout;
pub mod psci;
pub mod sysreg;
