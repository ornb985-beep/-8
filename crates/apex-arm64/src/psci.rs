//! PSCI 1.1 / SMCCC 1.1 firmware interface.
//!
//! There is no Trusted Firmware in an Apex VM: the VMM itself answers the
//! guest's `hvc #0` calls. Besides CPU power management this also implements
//! the SMCCC TRNG interface so the guest kernel gets high quality entropy
//! before any driver has probed (faster `crng init` on Android boot).

pub mod fid {
    pub const PSCI_VERSION: u32 = 0x8400_0000;
    pub const CPU_SUSPEND: u32 = 0x8400_0001;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON: u32 = 0x8400_0003;
    pub const AFFINITY_INFO: u32 = 0x8400_0004;
    pub const MIGRATE: u32 = 0x8400_0005;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const MIGRATE_INFO_UP_CPU: u32 = 0x8400_0007;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;
    pub const PSCI_FEATURES: u32 = 0x8400_000a;
    pub const CPU_FREEZE: u32 = 0x8400_000b;
    pub const CPU_DEFAULT_SUSPEND: u32 = 0x8400_000c;
    pub const SYSTEM_SUSPEND: u32 = 0x8400_000e;
    pub const SYSTEM_RESET2: u32 = 0x8400_0012;

    pub const SMCCC_VERSION: u32 = 0x8000_0000;
    pub const SMCCC_ARCH_FEATURES: u32 = 0x8000_0001;
    pub const SMCCC_ARCH_SOC_ID: u32 = 0x8000_0002;
    pub const SMCCC_ARCH_WORKAROUND_1: u32 = 0x8000_8000;
    pub const SMCCC_ARCH_WORKAROUND_2: u32 = 0x8000_7fff;
    pub const SMCCC_ARCH_WORKAROUND_3: u32 = 0x8000_3fff;

    pub const TRNG_VERSION: u32 = 0x8400_0050;
    pub const TRNG_FEATURES: u32 = 0x8400_0051;
    pub const TRNG_GET_UUID: u32 = 0x8400_0052;
    pub const TRNG_RND32: u32 = 0x8400_0053;
    pub const TRNG_RND64: u32 = 0xc400_0053;

    /// Bit 30 selects the SMC64 calling convention.
    pub const SMC64: u32 = 0x4000_0000;
}

pub mod ret {
    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMETERS: i64 = -2;
    pub const DENIED: i64 = -3;
    pub const ALREADY_ON: i64 = -4;
    pub const ON_PENDING: i64 = -5;
    pub const INTERNAL_FAILURE: i64 = -6;
    pub const NOT_PRESENT: i64 = -7;
    pub const INVALID_ADDRESS: i64 = -9;
    /// SMCCC_ARCH_FEATURES: the CPU is not affected by the erratum.
    pub const WORKAROUND_UNAFFECTED: i64 = 1;
    pub const NOT_REQUIRED: i64 = -2;
    /// TRNG: not enough entropy right now.
    pub const TRNG_NO_ENTROPY: i64 = -3;
}

/// AFFINITY_INFO results.
pub const AFF_ON: i64 = 0;
pub const AFF_OFF: i64 = 1;
pub const AFF_ON_PENDING: i64 = 2;

/// UUID identifying the Apex TRNG back end (random, stable).
pub const TRNG_UUID: [u32; 4] = [0x41504558, 0x2d54524e, 0x47a1b2c3, 0x0d15ea5e];

/// Services the VMM must provide to the PSCI implementation.
pub trait PsciPlatform {
    /// Power on the CPU with `mpidr` at `entry` with x0 = `context`.
    fn cpu_on(&self, mpidr: u64, entry: u64, context: u64) -> i64;
    fn affinity_info(&self, mpidr: u64) -> i64;
    fn fill_random(&self, buf: &mut [u8]) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsciOutcome {
    /// Write x0..x3 and resume the guest.
    Return([u64; 4]),
    /// The calling CPU powers down; it may be revived by CPU_ON.
    CpuOff,
    /// Idle the CPU until an interrupt (CPU_SUSPEND standby state).
    Suspend,
    SystemOff,
    SystemReset,
}

fn r(v: i64) -> PsciOutcome {
    PsciOutcome::Return([v as u64, 0, 0, 0])
}

/// Dispatch a PSCI/SMCCC call. `x` holds x0..x3 at the time of the call.
pub fn handle(x: [u64; 4], plat: &dyn PsciPlatform) -> PsciOutcome {
    let fid_full = x[0] as u32;
    // Normalize SMC64 IDs onto their SMC32 twins where the semantics match.
    let fid = if fid_full & 0xff00_0000 == 0xc400_0000 { fid_full & !fid::SMC64 } else { fid_full };
    let is64 = fid_full & fid::SMC64 != 0;
    let arg = |i: usize| if is64 { x[i] } else { x[i] & 0xffff_ffff };
    match fid {
        fid::PSCI_VERSION => r(0x0001_0001), // PSCI 1.1
        fid::CPU_SUSPEND => PsciOutcome::Suspend,
        fid::CPU_OFF => PsciOutcome::CpuOff,
        fid::CPU_ON => r(plat.cpu_on(arg(1), arg(2), arg(3))),
        fid::AFFINITY_INFO => {
            if arg(2) != 0 {
                r(ret::INVALID_PARAMETERS)
            } else {
                r(plat.affinity_info(arg(1)))
            }
        }
        fid::MIGRATE_INFO_TYPE => r(2), // Trusted OS not present / MP
        fid::MIGRATE | fid::MIGRATE_INFO_UP_CPU => r(ret::NOT_SUPPORTED),
        fid::SYSTEM_OFF => PsciOutcome::SystemOff,
        fid::SYSTEM_RESET => PsciOutcome::SystemReset,
        fid::SYSTEM_RESET2 => PsciOutcome::SystemReset,
        fid::PSCI_FEATURES => {
            let q = x[1] as u32;
            let qn = if q & 0xff00_0000 == 0xc400_0000 { q & !fid::SMC64 } else { q };
            match qn {
                fid::PSCI_VERSION
                | fid::CPU_OFF
                | fid::CPU_ON
                | fid::AFFINITY_INFO
                | fid::MIGRATE_INFO_TYPE
                | fid::SYSTEM_OFF
                | fid::SYSTEM_RESET
                | fid::SYSTEM_RESET2
                | fid::PSCI_FEATURES
                | fid::SMCCC_VERSION => r(0),
                // Feature flags for CPU_SUSPEND: original power_state format.
                fid::CPU_SUSPEND => r(0),
                _ => r(ret::NOT_SUPPORTED),
            }
        }
        fid::SMCCC_VERSION => r(0x0001_0001),
        fid::SMCCC_ARCH_FEATURES => match x[1] as u32 {
            fid::SMCCC_ARCH_WORKAROUND_1 | fid::SMCCC_ARCH_WORKAROUND_3 => r(ret::WORKAROUND_UNAFFECTED),
            fid::SMCCC_ARCH_WORKAROUND_2 => r(ret::NOT_REQUIRED),
            _ => r(ret::NOT_SUPPORTED),
        },
        fid::TRNG_VERSION => r(0x0001_0000),
        fid::TRNG_FEATURES => match x[1] as u32 {
            fid::TRNG_VERSION | fid::TRNG_FEATURES | fid::TRNG_GET_UUID | fid::TRNG_RND32 | fid::TRNG_RND64 => r(0),
            _ => r(ret::NOT_SUPPORTED),
        },
        fid::TRNG_GET_UUID => PsciOutcome::Return([TRNG_UUID[0] as u64, TRNG_UUID[1] as u64, TRNG_UUID[2] as u64, TRNG_UUID[3] as u64]),
        fid::TRNG_RND32 => {
            // Only reached for the SMC32 form (SMC64 is handled below).
            if is64 {
                return trng64(arg(1), plat);
            }
            let bits = arg(1);
            if bits == 0 || bits > 96 {
                return r(ret::INVALID_PARAMETERS);
            }
            let mut b = [0u8; 12];
            if !plat.fill_random(&mut b) {
                return r(ret::TRNG_NO_ENTROPY);
            }
            let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
            let (mut x1, mut x2, mut x3) = (w(0), w(1), w(2));
            mask_bits(&mut [&mut x3, &mut x2, &mut x1], bits, 32);
            PsciOutcome::Return([0, x1, x2, x3])
        }
        _ => r(ret::NOT_SUPPORTED),
    }
}

fn trng64(bits: u64, plat: &dyn PsciPlatform) -> PsciOutcome {
    if bits == 0 || bits > 192 {
        return r(ret::INVALID_PARAMETERS);
    }
    let mut b = [0u8; 24];
    if !plat.fill_random(&mut b) {
        return r(ret::TRNG_NO_ENTROPY);
    }
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (mut x1, mut x2, mut x3) = (w(0), w(1), w(2));
    mask_bits(&mut [&mut x3, &mut x2, &mut x1], bits, 64);
    PsciOutcome::Return([0, x1, x2, x3])
}

/// Keep only the lowest `bits` bits spread over registers ordered from least
/// to most significant, each `width` bits wide.
fn mask_bits(regs: &mut [&mut u64; 3], bits: u64, width: u64) {
    let mut remaining = bits;
    for reg in regs.iter_mut() {
        let keep = remaining.min(width);
        **reg = if keep == 0 {
            0
        } else if keep == 64 {
            **reg
        } else {
            **reg & ((1u64 << keep) - 1)
        };
        remaining -= keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Plat {
        on: RefCell<Vec<(u64, u64, u64)>>,
    }
    impl PsciPlatform for Plat {
        fn cpu_on(&self, mpidr: u64, entry: u64, ctx: u64) -> i64 {
            self.on.borrow_mut().push((mpidr, entry, ctx));
            if mpidr == 0 {
                ret::ALREADY_ON
            } else {
                ret::SUCCESS
            }
        }
        fn affinity_info(&self, mpidr: u64) -> i64 {
            if mpidr == 0 {
                AFF_ON
            } else {
                AFF_OFF
            }
        }
        fn fill_random(&self, buf: &mut [u8]) -> bool {
            buf.fill(0xff);
            true
        }
    }

    #[test]
    fn version_and_power() {
        let p = Plat::default();
        assert_eq!(handle([fid::PSCI_VERSION as u64, 0, 0, 0], &p), PsciOutcome::Return([0x10001, 0, 0, 0]));
        assert_eq!(handle([fid::SYSTEM_OFF as u64, 0, 0, 0], &p), PsciOutcome::SystemOff);
        assert_eq!(handle([fid::SYSTEM_RESET as u64, 0, 0, 0], &p), PsciOutcome::SystemReset);
        assert_eq!(handle([fid::CPU_OFF as u64, 0, 0, 0], &p), PsciOutcome::CpuOff);
    }

    #[test]
    fn cpu_on_smc64_passes_full_width_args() {
        let p = Plat::default();
        let out = handle([0xc400_0003, 1, 0x8_0000_1000, 0xdead_beef_0000], &p);
        assert_eq!(out, PsciOutcome::Return([0, 0, 0, 0]));
        assert_eq!(p.on.borrow()[0], (1, 0x8_0000_1000, 0xdead_beef_0000));
        let out = handle([0xc400_0003, 0, 0, 0], &p);
        assert_eq!(out, PsciOutcome::Return([ret::ALREADY_ON as u64, 0, 0, 0]));
        // SMC32 variant truncates arguments.
        handle([0x8400_0003, 0x1_0000_0002, 0x1_0000_2000, 0], &p);
        assert_eq!(p.on.borrow()[2], (2, 0x2000, 0));
    }

    #[test]
    fn features_and_unknown() {
        let p = Plat::default();
        assert_eq!(handle([fid::PSCI_FEATURES as u64, 0xc400_0003, 0, 0], &p), PsciOutcome::Return([0, 0, 0, 0]));
        assert_eq!(handle([fid::PSCI_FEATURES as u64, fid::SYSTEM_SUSPEND as u64, 0, 0], &p), PsciOutcome::Return([u64::MAX, 0, 0, 0]));
        assert_eq!(handle([0x8600_ff01, 0, 0, 0], &p), PsciOutcome::Return([u64::MAX, 0, 0, 0]));
        assert_eq!(
            handle([fid::SMCCC_ARCH_FEATURES as u64, fid::SMCCC_ARCH_WORKAROUND_1 as u64, 0, 0], &p),
            PsciOutcome::Return([1, 0, 0, 0])
        );
    }

    #[test]
    fn trng_masks_bits() {
        let p = Plat::default();
        assert_eq!(handle([fid::TRNG_RND64 as u64, 72, 0, 0], &p), PsciOutcome::Return([0, 0, 0xff, u64::MAX]));
        assert_eq!(handle([fid::TRNG_RND32 as u64, 40, 0, 0], &p), PsciOutcome::Return([0, 0, 0xff, 0xffff_ffff]));
        assert_eq!(handle([fid::TRNG_RND64 as u64, 193, 0, 0], &p), PsciOutcome::Return([(-2i64) as u64, 0, 0, 0]));
    }
}
