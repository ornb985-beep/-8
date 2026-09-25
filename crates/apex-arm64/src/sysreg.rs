//! System register identifiers using the `hv_sys_reg_t` packing:
//! `op0<<14 | op1<<11 | CRn<<7 | CRm<<3 | op2` (op0 stored as its 2-bit value).

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SysReg(pub u16);

impl SysReg {
    pub const fn new(op0: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> SysReg {
        SysReg(
            ((op0 as u16 & 3) << 14) | ((op1 as u16 & 7) << 11) | ((crn as u16 & 0xf) << 7) | ((crm as u16 & 0xf) << 3) | (op2 as u16 & 7),
        )
    }
    pub const fn op0(self) -> u8 {
        (self.0 >> 14) as u8 & 3
    }
    pub const fn op1(self) -> u8 {
        (self.0 >> 11) as u8 & 7
    }
    pub const fn crn(self) -> u8 {
        (self.0 >> 7) as u8 & 0xf
    }
    pub const fn crm(self) -> u8 {
        (self.0 >> 3) as u8 & 0xf
    }
    pub const fn op2(self) -> u8 {
        self.0 as u8 & 7
    }
    /// Debug registers (op0 == 2) and the implementation-defined space.
    pub const fn is_debug(self) -> bool {
        self.op0() == 2
    }
    /// ICC_* GICv3 CPU interface registers (op0=3, op1=0, CRn=12, or PMR).
    pub const fn is_gic_cpuif(self) -> bool {
        (self.op0() == 3 && self.op1() == 0 && self.crn() == 12) || self.0 == ICC_PMR_EL1.0
    }
    /// Performance monitor registers (CRn=9 CRm=12..14 at EL0, CRn=14 CRm=8..15).
    pub const fn is_pmu(self) -> bool {
        self.op0() == 3 && ((self.crn() == 9 && self.crm() >= 12) || (self.op1() == 3 && self.crn() == 14 && self.crm() >= 8))
    }
}

impl std::fmt::Debug for SysReg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S{}_{}_C{}_C{}_{}", self.op0(), self.op1(), self.crn(), self.crm(), self.op2())
    }
}

macro_rules! regs {
    ($($name:ident = ($a:expr, $b:expr, $c:expr, $d:expr, $e:expr);)*) => {
        $(pub const $name: SysReg = SysReg::new($a, $b, $c, $d, $e);)*
    };
}

regs! {
    MIDR_EL1 = (3, 0, 0, 0, 0);
    MPIDR_EL1 = (3, 0, 0, 0, 5);
    SCTLR_EL1 = (3, 0, 1, 0, 0);
    CPACR_EL1 = (3, 0, 1, 0, 2);
    TTBR0_EL1 = (3, 0, 2, 0, 0);
    TTBR1_EL1 = (3, 0, 2, 0, 1);
    TCR_EL1 = (3, 0, 2, 0, 2);
    SPSR_EL1 = (3, 0, 4, 0, 0);
    ELR_EL1 = (3, 0, 4, 0, 1);
    SP_EL0 = (3, 0, 4, 1, 0);
    SP_EL1 = (3, 4, 4, 1, 0);
    ESR_EL1 = (3, 0, 5, 2, 0);
    FAR_EL1 = (3, 0, 6, 0, 0);
    VBAR_EL1 = (3, 0, 12, 0, 0);
    CNTKCTL_EL1 = (3, 0, 14, 1, 0);
    CNTFRQ_EL0 = (3, 3, 14, 0, 0);
    CNTPCT_EL0 = (3, 3, 14, 0, 1);
    CNTVCT_EL0 = (3, 3, 14, 0, 2);
    CNTP_TVAL_EL0 = (3, 3, 14, 2, 0);
    CNTP_CTL_EL0 = (3, 3, 14, 2, 1);
    CNTP_CVAL_EL0 = (3, 3, 14, 2, 2);
    CNTV_TVAL_EL0 = (3, 3, 14, 3, 0);
    CNTV_CTL_EL0 = (3, 3, 14, 3, 1);
    CNTV_CVAL_EL0 = (3, 3, 14, 3, 2);

    // Debug / OS lock (RAZ/WI for guests)
    OSLAR_EL1 = (2, 0, 1, 0, 4);
    OSLSR_EL1 = (2, 0, 1, 1, 4);
    OSDLR_EL1 = (2, 0, 1, 3, 4);
    MDSCR_EL1 = (2, 0, 0, 2, 2);
    MDCCINT_EL1 = (2, 0, 0, 2, 0);
    DBGPRCR_EL1 = (2, 0, 1, 4, 4);
    DBGCLAIMSET_EL1 = (2, 0, 7, 8, 6);
    DBGCLAIMCLR_EL1 = (2, 0, 7, 9, 6);

    // PMU
    PMCR_EL0 = (3, 3, 9, 12, 0);
    PMCNTENSET_EL0 = (3, 3, 9, 12, 1);
    PMCNTENCLR_EL0 = (3, 3, 9, 12, 2);
    PMOVSCLR_EL0 = (3, 3, 9, 12, 3);
    PMSELR_EL0 = (3, 3, 9, 12, 5);
    PMCEID0_EL0 = (3, 3, 9, 12, 6);
    PMCEID1_EL0 = (3, 3, 9, 12, 7);
    PMCCNTR_EL0 = (3, 3, 9, 13, 0);
    PMUSERENR_EL0 = (3, 3, 9, 14, 0);
    PMINTENSET_EL1 = (3, 0, 9, 14, 1);
    PMINTENCLR_EL1 = (3, 0, 9, 14, 2);
    PMCCFILTR_EL0 = (3, 3, 14, 15, 7);

    // GICv3 CPU interface
    ICC_PMR_EL1 = (3, 0, 4, 6, 0);
    ICC_IAR0_EL1 = (3, 0, 12, 8, 0);
    ICC_EOIR0_EL1 = (3, 0, 12, 8, 1);
    ICC_HPPIR0_EL1 = (3, 0, 12, 8, 2);
    ICC_BPR0_EL1 = (3, 0, 12, 8, 3);
    ICC_AP0R0_EL1 = (3, 0, 12, 8, 4);
    ICC_AP1R0_EL1 = (3, 0, 12, 9, 0);
    ICC_AP1R1_EL1 = (3, 0, 12, 9, 1);
    ICC_AP1R2_EL1 = (3, 0, 12, 9, 2);
    ICC_AP1R3_EL1 = (3, 0, 12, 9, 3);
    ICC_DIR_EL1 = (3, 0, 12, 11, 1);
    ICC_RPR_EL1 = (3, 0, 12, 11, 3);
    ICC_SGI1R_EL1 = (3, 0, 12, 11, 5);
    ICC_ASGI1R_EL1 = (3, 0, 12, 11, 6);
    ICC_SGI0R_EL1 = (3, 0, 12, 11, 7);
    ICC_IAR1_EL1 = (3, 0, 12, 12, 0);
    ICC_EOIR1_EL1 = (3, 0, 12, 12, 1);
    ICC_HPPIR1_EL1 = (3, 0, 12, 12, 2);
    ICC_BPR1_EL1 = (3, 0, 12, 12, 3);
    ICC_CTLR_EL1 = (3, 0, 12, 12, 4);
    ICC_SRE_EL1 = (3, 0, 12, 12, 5);
    ICC_IGRPEN0_EL1 = (3, 0, 12, 12, 6);
    ICC_IGRPEN1_EL1 = (3, 0, 12, 12, 7);
}

/// CNTx_CTL_EL0 bits.
pub mod cntctl {
    pub const ENABLE: u64 = 1 << 0;
    pub const IMASK: u64 = 1 << 1;
    pub const ISTATUS: u64 = 1 << 2;

    /// Timer output line: enabled, condition met and not masked.
    pub fn irq_asserted(ctl: u64) -> bool {
        ctl & (ENABLE | IMASK | ISTATUS) == (ENABLE | ISTATUS)
    }
    /// Whether the timer can fire in the future (used to arm WFI deadlines).
    pub fn armed(ctl: u64) -> bool {
        ctl & (ENABLE | IMASK) == ENABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_hypervisor_framework_values() {
        // Values from <Hypervisor/hv_vcpu_types.h>.
        assert_eq!(MPIDR_EL1.0, 0xc005);
        assert_eq!(SCTLR_EL1.0, 0xc080);
        assert_eq!(CNTV_CTL_EL0.0, 0xdf19);
        assert_eq!(CNTV_CVAL_EL0.0, 0xdf1a);
        assert_eq!(SP_EL1.0, 0xe208);
        assert_eq!(VBAR_EL1.0, 0xc600);
        assert_eq!(ELR_EL1.0, 0xc201);
        assert_eq!(SPSR_EL1.0, 0xc200);
        assert_eq!(MDSCR_EL1.0, 0x8012);
        assert_eq!(CNTKCTL_EL1.0, 0xc708);
    }

    #[test]
    fn classification() {
        assert!(ICC_IAR1_EL1.is_gic_cpuif());
        assert!(ICC_PMR_EL1.is_gic_cpuif());
        assert!(!SCTLR_EL1.is_gic_cpuif());
        assert!(PMCR_EL0.is_pmu());
        assert!(PMCCFILTR_EL0.is_pmu());
        assert!(PMINTENSET_EL1.is_pmu());
        assert!(OSLAR_EL1.is_debug());
        assert!(cntctl::irq_asserted(0b101));
        assert!(!cntctl::irq_asserted(0b111));
        assert!(cntctl::armed(0b001));
    }
}
