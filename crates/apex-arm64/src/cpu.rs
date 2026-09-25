//! vCPU identity and reset state.

/// PSTATE for kernel entry: EL1h, SError/IRQ/FIQ/Debug masked.
pub const PSTATE_EL1H_DAIF: u64 = 0x3c5;

/// SCTLR_EL1 architectural RES1 bits with the MMU and caches off, which is
/// what the arm64 Linux boot protocol requires at entry.
pub const SCTLR_EL1_RESET: u64 = 0x30d0_0800;

/// Affinity value for vCPU `index`. Aff0 is limited to 16 CPUs so that every
/// CPU can be addressed by a single ICC_SGI1R_EL1 target list (GICv3).
pub const fn mpidr_for(index: usize) -> u64 {
    let aff0 = (index % 16) as u64;
    let aff1 = ((index / 16) % 256) as u64;
    let aff2 = ((index / 4096) % 256) as u64;
    aff0 | (aff1 << 8) | (aff2 << 16)
}

/// Strip MT/U/RES1 bits leaving only Aff3..Aff0.
pub const fn affinity(mpidr: u64) -> u64 {
    mpidr & 0xff_00ff_ffff
}

/// Inverse of [`mpidr_for`].
pub fn index_for(mpidr: u64, cpus: usize) -> Option<usize> {
    let a = affinity(mpidr);
    (0..cpus).find(|&i| mpidr_for(i) == a)
}

/// GICR_TYPER / ICC_SGI1R style affinity packing Aff3.Aff2.Aff1.Aff0 as 32 bits.
pub const fn packed_affinity(mpidr: u64) -> u32 {
    let a = affinity(mpidr);
    ((a & 0xff_ffff) | ((a >> 32 & 0xff) << 24)) as u32
}

/// Register state a CPU starts executing with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryState {
    pub pc: u64,
    pub x0: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpidr_roundtrip() {
        for i in 0..64 {
            assert_eq!(index_for(mpidr_for(i), 64), Some(i));
            assert_eq!(index_for(mpidr_for(i) | (1 << 31), 64), Some(i));
        }
        assert_eq!(mpidr_for(17), 0x101);
        assert_eq!(packed_affinity(0x01_0002_0304), 0x0102_0304);
        assert_eq!(index_for(0x5, 4), None);
    }
}
