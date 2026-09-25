//! Exception Syndrome Register (ESR_EL2) decoding.

/// Exception classes the VMM cares about.
pub mod ec {
    pub const UNKNOWN: u32 = 0x00;
    pub const WFX: u32 = 0x01;
    pub const FP_ACCESS: u32 = 0x07;
    pub const ILLEGAL_STATE: u32 = 0x0e;
    pub const SVC64: u32 = 0x15;
    pub const HVC64: u32 = 0x16;
    pub const SMC64: u32 = 0x17;
    pub const SYSREG: u32 = 0x18;
    pub const SVE: u32 = 0x19;
    pub const IABT_LOWER: u32 = 0x20;
    pub const IABT_CUR: u32 = 0x21;
    pub const PC_ALIGN: u32 = 0x22;
    pub const DABT_LOWER: u32 = 0x24;
    pub const DABT_CUR: u32 = 0x25;
    pub const SP_ALIGN: u32 = 0x26;
    pub const FP_EXC64: u32 = 0x2c;
    pub const BREAKPT_LOWER: u32 = 0x30;
    pub const SOFTSTP_LOWER: u32 = 0x32;
    pub const WATCHPT_LOWER: u32 = 0x34;
    pub const BRK64: u32 = 0x3c;

    pub fn name(ec: u32) -> &'static str {
        match ec {
            UNKNOWN => "unknown",
            WFX => "wfi/wfe",
            FP_ACCESS => "fp-access",
            ILLEGAL_STATE => "illegal-state",
            SVC64 => "svc64",
            HVC64 => "hvc64",
            SMC64 => "smc64",
            SYSREG => "msr/mrs",
            SVE => "sve",
            IABT_LOWER => "iabt-lower",
            IABT_CUR => "iabt-cur",
            PC_ALIGN => "pc-align",
            DABT_LOWER => "dabt-lower",
            DABT_CUR => "dabt-cur",
            SP_ALIGN => "sp-align",
            FP_EXC64 => "fp-exc64",
            BREAKPT_LOWER => "breakpoint",
            SOFTSTP_LOWER => "software-step",
            WATCHPT_LOWER => "watchpoint",
            BRK64 => "brk64",
            _ => "?",
        }
    }
}

#[inline]
pub fn exception_class(esr: u64) -> u32 {
    ((esr >> 26) & 0x3f) as u32
}

#[inline]
pub fn iss(esr: u64) -> u32 {
    (esr & 0x01ff_ffff) as u32
}

/// Instruction length bit: 1 = 32-bit instruction.
#[inline]
pub fn il32(esr: u64) -> bool {
    esr & (1 << 25) != 0
}

/// Decoded data abort with a valid instruction syndrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataAbort {
    /// Instruction syndrome valid: without it the VMM would have to decode
    /// the faulting instruction itself.
    pub isv: bool,
    /// Access size in bytes (1, 2, 4, 8).
    pub size: usize,
    pub sign_extend: bool,
    /// Transfer register; 31 means XZR for loads/stores.
    pub srt: u8,
    /// Destination is a 64-bit register.
    pub sf: bool,
    pub acquire_release: bool,
    pub write: bool,
    /// Stage-1 page table walk fault (never MMIO).
    pub s1ptw: bool,
    pub dfsc: u8,
}

impl DataAbort {
    pub fn decode(esr: u64) -> DataAbort {
        let iss = iss(esr);
        DataAbort {
            isv: iss & (1 << 24) != 0,
            size: 1 << ((iss >> 22) & 3),
            sign_extend: iss & (1 << 21) != 0,
            srt: ((iss >> 16) & 0x1f) as u8,
            sf: iss & (1 << 15) != 0,
            acquire_release: iss & (1 << 14) != 0,
            write: iss & (1 << 6) != 0,
            s1ptw: iss & (1 << 7) != 0,
            dfsc: (iss & 0x3f) as u8,
        }
    }

    /// Fix up a value read from a device before it is written to `srt`
    /// (sign extension and 32-bit truncation).
    pub fn load_value(&self, raw: u64) -> u64 {
        let bits = self.size * 8;
        let mut v = if bits == 64 { raw } else { raw & ((1u64 << bits) - 1) };
        if self.sign_extend && bits < 64 {
            let shift = 64 - bits;
            v = (((v << shift) as i64) >> shift) as u64;
        }
        if !self.sf {
            v &= 0xffff_ffff;
        }
        v
    }
}

/// Decoded MSR/MRS trap (EC 0x18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysRegAccess {
    pub op0: u8,
    pub op1: u8,
    pub crn: u8,
    pub crm: u8,
    pub op2: u8,
    pub rt: u8,
    /// true for MRS (register read by the guest).
    pub read: bool,
}

impl SysRegAccess {
    pub fn decode(esr: u64) -> SysRegAccess {
        let iss = iss(esr);
        SysRegAccess {
            op0: ((iss >> 20) & 3) as u8,
            op2: ((iss >> 17) & 7) as u8,
            op1: ((iss >> 14) & 7) as u8,
            crn: ((iss >> 10) & 0xf) as u8,
            rt: ((iss >> 5) & 0x1f) as u8,
            crm: ((iss >> 1) & 0xf) as u8,
            read: iss & 1 != 0,
        }
    }

    pub fn id(&self) -> crate::sysreg::SysReg {
        crate::sysreg::SysReg::new(self.op0, self.op1, self.crn, self.crm, self.op2)
    }
}

/// WFI vs WFE (and the timeout variants from FEAT_WFxT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wfx {
    Wfi,
    Wfe,
    Wfit,
    Wfet,
}

impl Wfx {
    pub fn decode(esr: u64) -> Wfx {
        match iss(esr) & 3 {
            0 => Wfx::Wfi,
            1 => Wfx::Wfe,
            2 => Wfx::Wfit,
            _ => Wfx::Wfet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esr(ec: u32, iss: u32) -> u64 {
        ((ec as u64) << 26) | (1 << 25) | iss as u64
    }

    #[test]
    fn data_abort_str_w1_to_x3() {
        // ISV=1, SAS=2 (word), SRT=3, WnR=1
        let e = esr(ec::DABT_LOWER, (1 << 24) | (2 << 22) | (3 << 16) | (1 << 6));
        assert_eq!(exception_class(e), ec::DABT_LOWER);
        let d = DataAbort::decode(e);
        assert!(d.isv && d.write);
        assert_eq!(d.size, 4);
        assert_eq!(d.srt, 3);
    }

    #[test]
    fn load_sign_extension() {
        // LDRSB x0: SAS=0 SSE=1 SF=1
        let d = DataAbort::decode(esr(ec::DABT_LOWER, (1 << 24) | (1 << 21) | (1 << 15)));
        assert_eq!(d.load_value(0x80), 0xffff_ffff_ffff_ff80);
        // LDRSH w0: SAS=1 SSE=1 SF=0
        let d = DataAbort::decode(esr(ec::DABT_LOWER, (1 << 24) | (1 << 22) | (1 << 21)));
        assert_eq!(d.load_value(0x8001), 0xffff_8001);
        // LDR w0 zero-extends and truncates garbage above the access size
        let d = DataAbort::decode(esr(ec::DABT_LOWER, (1 << 24) | (2 << 22)));
        assert_eq!(d.load_value(0xdead_beef_1234_5678), 0x1234_5678);
    }

    #[test]
    fn sysreg_decode_icc_iar1() {
        // MRS x5, ICC_IAR1_EL1 : op0=3 op1=0 CRn=12 CRm=12 op2=0
        let iss = (3 << 20) | (12 << 10) | (5 << 5) | (12 << 1) | 1; // op1 = op2 = 0
        let s = SysRegAccess::decode(esr(ec::SYSREG, iss));
        assert_eq!((s.op0, s.op1, s.crn, s.crm, s.op2, s.rt, s.read), (3, 0, 12, 12, 0, 5, true));
        assert_eq!(s.id(), crate::sysreg::ICC_IAR1_EL1);
    }
}
