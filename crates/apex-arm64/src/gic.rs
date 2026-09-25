//! Userspace GICv3 model (distributor, redistributors and the ICC_* system
//! register CPU interface).
//!
//! On macOS 15+ Apex uses Apple's in-kernel vGICv3 and this model is idle.
//! On older hosts (or with `gic = "emulated"`) the guest's ICC_* accesses
//! trap to the VMM and are served here; the resulting IRQ line is driven into
//! the vCPU with `hv_vcpu_set_pending_interrupt`.
//!
//! Scope: single security state (GICD_CTLR.DS=1), affinity routing only
//! (ARE=1), Group 1 interrupts, 5 priority bits, no LPIs/ITS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use apex_core::bus::MmioDevice;
use apex_core::irq::InterruptController;
use apex_core::mem::{le_read, le_write};

use crate::cpu::{affinity, mpidr_for, packed_affinity};
use crate::sysreg::{self, SysReg};

pub const SPURIOUS: u32 = 1023;
const PRIO_MASK: u8 = 0xf8; // 5 implemented priority bits
const IIDR: u32 = 0x0100_043b; // implementer ARM, product "Apex"
const PIDR2_GICV3: u32 = 0x3b;

pub const GICD_SIZE: u64 = 0x1_0000;
pub const GICR_STRIDE: u64 = 0x2_0000;
const SGI_BASE: u64 = 0x1_0000;

#[derive(Clone, Copy, Debug, Default)]
struct Irq {
    enabled: bool,
    /// Software/edge pending latch.
    latch: bool,
    /// Input line level (level-sensitive sources).
    level: bool,
    active: bool,
    edge: bool,
    group1: bool,
    priority: u8,
    /// IROUTER value (SPIs only).
    route: u64,
}

impl Irq {
    #[inline]
    fn pending(&self) -> bool {
        self.latch || (!self.edge && self.level)
    }
}

#[derive(Clone, Debug)]
struct Cpu {
    private: [Irq; 32],
    waker_sleep: bool,
    pmr: u8,
    bpr1: u8,
    eoimode: bool,
    cbpr: bool,
    igrpen1: bool,
    /// Active priority stack: (priority, intid), highest priority last.
    active_prios: Vec<(u8, u32)>,
}

impl Cpu {
    fn new() -> Self {
        let mut private = [Irq::default(); 32];
        for (i, irq) in private.iter_mut().enumerate() {
            irq.edge = i < 16; // SGIs are always edge
        }
        Cpu { private, waker_sleep: true, pmr: 0, bpr1: 3, eoimode: false, cbpr: false, igrpen1: false, active_prios: Vec::new() }
    }

    fn running_priority(&self) -> u8 {
        self.active_prios.iter().map(|p| p.0).min().unwrap_or(0xff)
    }

    /// Group priority mask used for preemption. For Group 1 the group
    /// priority field is bits [7:BPR1]; with 5 priority bits the minimum
    /// meaningful BPR1 is 3.
    fn group_mask(&self) -> u8 {
        let bpr = self.bpr1.clamp(3, 7);
        ((0xffu32 << bpr) & 0xff) as u8
    }
}

struct State {
    ctlr_grp1: bool,
    spis: Vec<Irq>,
    cpus: Vec<Cpu>,
}

pub type Notifier = Box<dyn Fn(usize) + Send + Sync>;

pub struct GicV3 {
    state: Mutex<State>,
    ncpus: usize,
    nirqs: u32,
    lines: Vec<AtomicBool>,
    notifier: RwLock<Option<Notifier>>,
}

impl GicV3 {
    /// `spi_count` is rounded up to a multiple of 32.
    pub fn new(ncpus: usize, spi_count: u32) -> Arc<GicV3> {
        let spis = spi_count.div_ceil(32) * 32;
        let nirqs = (32 + spis).min(1020);
        let st =
            State { ctlr_grp1: false, spis: vec![Irq::default(); (nirqs - 32) as usize], cpus: (0..ncpus).map(|_| Cpu::new()).collect() };
        Arc::new(GicV3 {
            state: Mutex::new(st),
            ncpus,
            nirqs,
            lines: (0..ncpus).map(|_| AtomicBool::new(false)).collect(),
            notifier: RwLock::new(None),
        })
    }

    pub fn num_irqs(&self) -> u32 {
        self.nirqs
    }

    pub fn num_cpus(&self) -> usize {
        self.ncpus
    }

    /// Called with a CPU index whenever that CPU's IRQ line rises (so the
    /// VMM can kick the vCPU out of guest mode or out of WFI).
    pub fn set_notifier(&self, n: Notifier) {
        *self.notifier.write().unwrap() = Some(n);
    }

    /// Current IRQ line of `cpu` (cheap; read before every `hv_vcpu_run`).
    #[inline]
    pub fn irq_line(&self, cpu: usize) -> bool {
        self.lines[cpu].load(Ordering::Acquire)
    }

    fn irq_mut(st: &mut State, cpu: usize, intid: u32) -> Option<&mut Irq> {
        if intid < 32 {
            st.cpus.get_mut(cpu).map(|c| &mut c.private[intid as usize])
        } else {
            st.spis.get_mut((intid - 32) as usize)
        }
    }

    fn spi_target(&self, route: u64) -> usize {
        if route & (1 << 31) != 0 {
            return 0; // 1-of-N: deliver to CPU 0
        }
        let a = affinity(route);
        (0..self.ncpus).find(|&c| mpidr_for(c) == a).unwrap_or(0)
    }

    /// Highest priority pending interrupt for `cpu`: (intid, priority).
    fn hppi(&self, st: &State, cpu: usize) -> Option<(u32, u8)> {
        let c = &st.cpus[cpu];
        let mut best: Option<(u32, u8)> = None;
        let mut consider = |intid: u32, irq: &Irq| {
            if irq.enabled && irq.group1 && irq.pending() && !irq.active && best.is_none_or(|(_, p)| irq.priority < p) {
                best = Some((intid, irq.priority));
            }
        };
        for (i, irq) in c.private.iter().enumerate() {
            consider(i as u32, irq);
        }
        for (i, irq) in st.spis.iter().enumerate() {
            if irq.enabled && irq.pending() && self.spi_target(irq.route) == cpu {
                consider(i as u32 + 32, irq);
            }
        }
        best
    }

    fn deliverable(&self, st: &State, cpu: usize) -> bool {
        if !st.ctlr_grp1 || !st.cpus[cpu].igrpen1 {
            return false;
        }
        let c = &st.cpus[cpu];
        match self.hppi(st, cpu) {
            Some((_, prio)) => {
                let gm = c.group_mask();
                prio < c.pmr && (prio & gm) < (c.running_priority() & gm)
            }
            None => false,
        }
    }

    /// Recompute IRQ lines; notify CPUs whose line rose.
    fn update(&self, st: &State) {
        let mut raised = Vec::new();
        for cpu in 0..self.ncpus {
            let d = self.deliverable(st, cpu);
            let prev = self.lines[cpu].swap(d, Ordering::AcqRel);
            if d && !prev {
                raised.push(cpu);
            }
        }
        if !raised.is_empty() {
            if let Some(n) = self.notifier.read().unwrap().as_ref() {
                for c in raised {
                    n(c);
                }
            }
        }
    }

    fn update_one(&self, st: &State, cpu: usize) {
        let d = self.deliverable(st, cpu);
        let prev = self.lines[cpu].swap(d, Ordering::AcqRel);
        if d && !prev {
            if let Some(n) = self.notifier.read().unwrap().as_ref() {
                n(cpu);
            }
        }
    }

    /// Drive a PPI (e.g. the virtual timer, INTID 27) for one CPU.
    pub fn set_ppi_level(&self, cpu: usize, intid: u32, level: bool) {
        debug_assert!((16..32).contains(&intid));
        let mut st = self.state.lock().unwrap();
        let irq = &mut st.cpus[cpu].private[intid as usize];
        if irq.edge && level && !irq.level {
            irq.latch = true;
        }
        irq.level = level;
        self.update_one(&st, cpu);
    }

    pub fn set_spi(&self, intid: u32, level: bool) {
        let mut st = self.state.lock().unwrap();
        let Some(irq) = st.spis.get_mut((intid.wrapping_sub(32)) as usize) else { return };
        if irq.edge && level && !irq.level {
            irq.latch = true;
        }
        irq.level = level;
        self.update(&st);
    }

    // ------------------------------------------------------------------
    // CPU interface (ICC_* system registers)
    // ------------------------------------------------------------------

    /// Handle an MRS from `cpu`. Returns None if `reg` is not an ICC register.
    pub fn sysreg_read(&self, cpu: usize, reg: SysReg) -> Option<u64> {
        if !reg.is_gic_cpuif() {
            return None;
        }
        let mut st = self.state.lock().unwrap();
        let v = match reg {
            sysreg::ICC_IAR1_EL1 => self.acknowledge(&mut st, cpu) as u64,
            sysreg::ICC_HPPIR1_EL1 => self.hppi(&st, cpu).map(|h| h.0).unwrap_or(SPURIOUS) as u64,
            sysreg::ICC_PMR_EL1 => st.cpus[cpu].pmr as u64,
            sysreg::ICC_RPR_EL1 => st.cpus[cpu].running_priority() as u64,
            sysreg::ICC_BPR1_EL1 => st.cpus[cpu].bpr1.max(3) as u64,
            sysreg::ICC_CTLR_EL1 => {
                let c = &st.cpus[cpu];
                // PRIbits=4 (5 bits), IDbits=0 (16 bits), EOImode, CBPR.
                (4u64 << 8) | ((c.eoimode as u64) << 1) | c.cbpr as u64
            }
            sysreg::ICC_SRE_EL1 => 0x7,
            sysreg::ICC_IGRPEN1_EL1 => st.cpus[cpu].igrpen1 as u64,
            sysreg::ICC_AP1R0_EL1 => {
                let mut bits = 0u64;
                for &(p, _) in &st.cpus[cpu].active_prios {
                    bits |= 1 << (p >> 3);
                }
                bits
            }
            sysreg::ICC_IAR0_EL1 | sysreg::ICC_HPPIR0_EL1 => SPURIOUS as u64,
            _ => 0,
        };
        self.update(&st);
        Some(v)
    }

    /// Handle an MSR from `cpu`. Returns false if `reg` is not an ICC register.
    pub fn sysreg_write(&self, cpu: usize, reg: SysReg, val: u64) -> bool {
        if !reg.is_gic_cpuif() {
            return false;
        }
        let mut st = self.state.lock().unwrap();
        match reg {
            sysreg::ICC_EOIR1_EL1 => {
                let intid = (val & 0xff_ffff) as u32;
                let c = &mut st.cpus[cpu];
                if let Some(pos) = c.active_prios.iter().rposition(|&(_, i)| i == intid) {
                    c.active_prios.remove(pos);
                }
                if !st.cpus[cpu].eoimode {
                    Self::deactivate(&mut st, cpu, intid);
                }
            }
            sysreg::ICC_DIR_EL1 => Self::deactivate(&mut st, cpu, (val & 0xff_ffff) as u32),
            sysreg::ICC_PMR_EL1 => st.cpus[cpu].pmr = val as u8 & PRIO_MASK,
            sysreg::ICC_BPR1_EL1 => st.cpus[cpu].bpr1 = (val & 7) as u8,
            sysreg::ICC_CTLR_EL1 => {
                st.cpus[cpu].eoimode = val & 2 != 0;
                st.cpus[cpu].cbpr = val & 1 != 0;
            }
            sysreg::ICC_IGRPEN1_EL1 => st.cpus[cpu].igrpen1 = val & 1 != 0,
            sysreg::ICC_AP1R0_EL1 => {
                if val == 0 {
                    st.cpus[cpu].active_prios.clear();
                }
            }
            sysreg::ICC_SGI1R_EL1 | sysreg::ICC_ASGI1R_EL1 | sysreg::ICC_SGI0R_EL1 => {
                self.generate_sgi(&mut st, cpu, val);
            }
            _ => {} // group 0 and SRE writes are ignored
        }
        self.update(&st);
        true
    }

    fn deactivate(st: &mut State, cpu: usize, intid: u32) {
        if let Some(irq) = Self::irq_mut(st, cpu, intid) {
            irq.active = false;
        }
    }

    fn acknowledge(&self, st: &mut State, cpu: usize) -> u32 {
        if !self.deliverable(st, cpu) {
            return SPURIOUS;
        }
        let Some((intid, prio)) = self.hppi(st, cpu) else { return SPURIOUS };
        let irq = Self::irq_mut(st, cpu, intid).unwrap();
        irq.active = true;
        irq.latch = false;
        st.cpus[cpu].active_prios.push((prio, intid));
        intid
    }

    fn generate_sgi(&self, st: &mut State, from: usize, val: u64) {
        let intid = ((val >> 24) & 0xf) as usize;
        let irm = val & (1 << 40) != 0;
        let targets = val & 0xffff;
        let aff1 = (val >> 16) & 0xff;
        let aff2 = (val >> 32) & 0xff;
        let aff3 = (val >> 48) & 0xff;
        let rs = (val >> 44) & 0xf;
        for cpu in 0..self.ncpus {
            let hit = if irm {
                cpu != from
            } else {
                let m = mpidr_for(cpu);
                let a0 = m & 0xff;
                (m >> 8) & 0xff == aff1
                    && (m >> 16) & 0xff == aff2
                    && (m >> 32) & 0xff == aff3
                    && a0 / 16 == rs
                    && targets & (1 << (a0 % 16)) != 0
            };
            if hit {
                st.cpus[cpu].private[intid].latch = true;
            }
        }
    }

    // ------------------------------------------------------------------
    // Distributor MMIO
    // ------------------------------------------------------------------

    fn bitfield_read(&self, st: &State, cpu: Option<usize>, base_intid: u32, f: impl Fn(&Irq) -> bool) -> u32 {
        let mut v = 0u32;
        for b in 0..32 {
            let intid = base_intid + b;
            let irq = if intid < 32 {
                match cpu {
                    Some(c) => &st.cpus[c].private[intid as usize],
                    None => continue,
                }
            } else {
                match st.spis.get((intid - 32) as usize) {
                    Some(i) => i,
                    None => continue,
                }
            };
            if f(irq) {
                v |= 1 << b;
            }
        }
        v
    }

    fn bitfield_write(st: &mut State, cpu: Option<usize>, base_intid: u32, val: u32, f: impl Fn(&mut Irq)) {
        for b in 0..32 {
            if val & (1 << b) == 0 {
                continue;
            }
            let intid = base_intid + b;
            let irq = if intid < 32 {
                match cpu {
                    Some(c) => &mut st.cpus[c].private[intid as usize],
                    None => continue,
                }
            } else {
                match st.spis.get_mut((intid - 32) as usize) {
                    Some(i) => i,
                    None => continue,
                }
            };
            f(irq);
        }
    }

    /// Shared register file between GICD (SPIs, cpu=None) and GICR SGI frame
    /// (private interrupts, cpu=Some). Returns None for unhandled offsets.
    fn irq_regs_read(&self, st: &State, cpu: Option<usize>, off: u64, len: usize) -> Option<u64> {
        let reg_base = |start: u64| ((off - start) / 4) as u32 * 32;
        Some(match off {
            0x080..=0x0fc => self.bitfield_read(st, cpu, reg_base(0x080), |i| i.group1) as u64,
            0x100..=0x17c => self.bitfield_read(st, cpu, reg_base(0x100), |i| i.enabled) as u64,
            0x180..=0x1fc => self.bitfield_read(st, cpu, reg_base(0x180), |i| i.enabled) as u64,
            0x200..=0x27c => self.bitfield_read(st, cpu, reg_base(0x200), |i| i.pending()) as u64,
            0x280..=0x2fc => self.bitfield_read(st, cpu, reg_base(0x280), |i| i.pending()) as u64,
            0x300..=0x37c => self.bitfield_read(st, cpu, reg_base(0x300), |i| i.active) as u64,
            0x380..=0x3fc => self.bitfield_read(st, cpu, reg_base(0x380), |i| i.active) as u64,
            0x400..=0x7fb => {
                let first = (off - 0x400) as u32;
                let mut v = 0u64;
                for k in 0..len.min(8) as u32 {
                    let intid = first + k;
                    let p = if intid < 32 {
                        cpu.map(|c| st.cpus[c].private[intid as usize].priority).unwrap_or(0)
                    } else {
                        st.spis.get((intid - 32) as usize).map(|i| i.priority).unwrap_or(0)
                    };
                    v |= (p as u64) << (8 * k);
                }
                v
            }
            0xc00..=0xcfc => {
                let first = ((off - 0xc00) / 4) as u32 * 16;
                let mut v = 0u32;
                for k in 0..16 {
                    let intid = first + k;
                    let edge = if intid < 32 {
                        cpu.map(|c| st.cpus[c].private[intid as usize].edge).unwrap_or(false)
                    } else {
                        st.spis.get((intid - 32) as usize).map(|i| i.edge).unwrap_or(false)
                    };
                    if edge {
                        v |= 2 << (2 * k);
                    }
                }
                v as u64
            }
            _ => return None,
        })
    }

    fn irq_regs_write(st: &mut State, cpu: Option<usize>, off: u64, data: &[u8]) -> bool {
        let val = le_read(data) as u32;
        let reg_base = |start: u64| ((off - start) / 4) as u32 * 32;
        match off {
            0x080..=0x0fc => {
                let base = reg_base(0x080);
                for b in 0..32 {
                    let intid = base + b;
                    let g = val & (1 << b) != 0;
                    if intid < 32 {
                        if let Some(c) = cpu {
                            st.cpus[c].private[intid as usize].group1 = g;
                        }
                    } else if let Some(i) = st.spis.get_mut((intid - 32) as usize) {
                        i.group1 = g;
                    }
                }
            }
            0x100..=0x17c => Self::bitfield_write(st, cpu, reg_base(0x100), val, |i| i.enabled = true),
            0x180..=0x1fc => Self::bitfield_write(st, cpu, reg_base(0x180), val, |i| i.enabled = false),
            0x200..=0x27c => Self::bitfield_write(st, cpu, reg_base(0x200), val, |i| i.latch = true),
            0x280..=0x2fc => Self::bitfield_write(st, cpu, reg_base(0x280), val, |i| i.latch = false),
            0x300..=0x37c => Self::bitfield_write(st, cpu, reg_base(0x300), val, |i| i.active = true),
            0x380..=0x3fc => Self::bitfield_write(st, cpu, reg_base(0x380), val, |i| i.active = false),
            0x400..=0x7fb => {
                let first = (off - 0x400) as u32;
                for (k, &b) in data.iter().enumerate() {
                    let intid = first + k as u32;
                    let p = b & PRIO_MASK;
                    if intid < 32 {
                        if let Some(c) = cpu {
                            st.cpus[c].private[intid as usize].priority = p;
                        }
                    } else if let Some(i) = st.spis.get_mut((intid - 32) as usize) {
                        i.priority = p;
                    }
                }
            }
            0xc00..=0xcfc => {
                let first = ((off - 0xc00) / 4) as u32 * 16;
                for k in 0..16 {
                    let intid = first + k;
                    let edge = val & (2 << (2 * k)) != 0;
                    if intid < 16 {
                        continue; // SGIs are always edge
                    }
                    if intid < 32 {
                        if let Some(c) = cpu {
                            st.cpus[c].private[intid as usize].edge = edge;
                        }
                    } else if let Some(i) = st.spis.get_mut((intid - 32) as usize) {
                        i.edge = edge;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    pub fn dist_read(&self, off: u64, data: &mut [u8]) {
        let st = self.state.lock().unwrap();
        let v: u64 = match off {
            0x0000 => ((st.ctlr_grp1 as u64) << 1) | (1 << 4) | (1 << 6), // EnableGrp1, ARE, DS
            0x0004 => {
                let it_lines = (self.nirqs / 32 - 1) as u64;
                it_lines | (9 << 19) // IDbits = 9 -> 10 bit INTIDs
            }
            0x0008 => IIDR as u64,
            0x000c | 0x0010 => 0,
            // Private interrupt banks are RAZ/WI in the distributor with ARE=1.
            0x080 | 0x100 | 0x180 | 0x200 | 0x280 | 0x300 | 0x380 | 0xc00 | 0xc04 => 0,
            0x400..=0x41f => 0,
            0x6000..=0x7fdf => {
                let n = ((off - 0x6000) / 8) as u32;
                let hi = off % 8 >= 4;
                let route = if n >= 32 { st.spis.get((n - 32) as usize).map(|i| i.route).unwrap_or(0) } else { 0 };
                if hi {
                    route >> 32
                } else {
                    route
                }
            }
            0xffe8 => PIDR2_GICV3 as u64,
            _ => self.irq_regs_read(&st, None, off, data.len()).unwrap_or(0),
        };
        le_write(data, v);
    }

    pub fn dist_write(&self, off: u64, data: &[u8]) {
        let mut st = self.state.lock().unwrap();
        let val = le_read(data);
        match off {
            0x0000 => st.ctlr_grp1 = val & 0b11 != 0,
            0x080 | 0x100 | 0x180 | 0x200 | 0x280 | 0x300 | 0x380 | 0xc00 | 0xc04 => {}
            0x400..=0x41f => {}
            0x6000..=0x7fdf => {
                let n = ((off - 0x6000) / 8) as u32;
                if n >= 32 {
                    if let Some(i) = st.spis.get_mut((n - 32) as usize) {
                        if data.len() == 8 {
                            i.route = val;
                        } else if off % 8 >= 4 {
                            i.route = (i.route & 0xffff_ffff) | (val << 32);
                        } else {
                            i.route = (i.route & !0xffff_ffff) | (val & 0xffff_ffff);
                        }
                    }
                }
            }
            _ => {
                Self::irq_regs_write(&mut st, None, off, data);
            }
        }
        self.update(&st);
    }

    // ------------------------------------------------------------------
    // Redistributor MMIO (all CPUs, contiguous frames of GICR_STRIDE)
    // ------------------------------------------------------------------

    pub fn redist_read(&self, off: u64, data: &mut [u8]) {
        let cpu = (off / GICR_STRIDE) as usize;
        let o = off % GICR_STRIDE;
        if cpu >= self.ncpus {
            le_write(data, 0);
            return;
        }
        let st = self.state.lock().unwrap();
        let v: u64 = if o < SGI_BASE {
            match o {
                0x0000 => 0,
                0x0004 => IIDR as u64,
                0x0008 | 0x000c => {
                    let last = (cpu == self.ncpus - 1) as u64;
                    let typer = ((packed_affinity(mpidr_for(cpu)) as u64) << 32) | ((cpu as u64) << 8) | (last << 4);
                    if o == 0x000c {
                        typer >> 32
                    } else {
                        typer
                    }
                }
                0x0014 => {
                    let s = st.cpus[cpu].waker_sleep as u64;
                    (s << 1) | (s << 2)
                }
                0xffe8 => PIDR2_GICV3 as u64,
                _ => 0,
            }
        } else {
            self.irq_regs_read(&st, Some(cpu), o - SGI_BASE, data.len()).unwrap_or(0)
        };
        le_write(data, v);
    }

    pub fn redist_write(&self, off: u64, data: &[u8]) {
        let cpu = (off / GICR_STRIDE) as usize;
        let o = off % GICR_STRIDE;
        if cpu >= self.ncpus {
            return;
        }
        let mut st = self.state.lock().unwrap();
        if o < SGI_BASE {
            if o == 0x0014 {
                st.cpus[cpu].waker_sleep = le_read(data) & 2 != 0;
            }
        } else {
            Self::irq_regs_write(&mut st, Some(cpu), o - SGI_BASE, data);
        }
        self.update(&st);
    }
}

impl InterruptController for GicV3 {
    fn set_spi_level(&self, spi: u32, level: bool) {
        self.set_spi(32 + spi, level);
    }
    fn spi_count(&self) -> u32 {
        self.nirqs - 32
    }
}

/// MMIO adapter for the distributor frame.
pub struct Distributor(pub Arc<GicV3>);
impl MmioDevice for Distributor {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.0.dist_read(offset, data)
    }
    fn write(&self, offset: u64, data: &[u8]) {
        self.0.dist_write(offset, data)
    }
    fn name(&self) -> &str {
        "gicv3-dist"
    }
}

/// MMIO adapter for all redistributor frames.
pub struct Redistributors(pub Arc<GicV3>);
impl MmioDevice for Redistributors {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.0.redist_read(offset, data)
    }
    fn write(&self, offset: u64, data: &[u8]) {
        self.0.redist_write(offset, data)
    }
    fn name(&self) -> &str {
        "gicv3-redist"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn w32(g: &GicV3, dist: bool, off: u64, v: u32) {
        if dist {
            g.dist_write(off, &v.to_le_bytes())
        } else {
            g.redist_write(off, &v.to_le_bytes())
        }
    }
    fn r32(g: &GicV3, dist: bool, off: u64) -> u32 {
        let mut b = [0u8; 4];
        if dist {
            g.dist_read(off, &mut b)
        } else {
            g.redist_read(off, &mut b)
        }
        u32::from_le_bytes(b)
    }

    /// Mimic the Linux gic-v3 driver init sequence for `n` CPUs.
    fn linux_init(g: &GicV3, n: usize) {
        w32(g, true, 0x0, 0); // disable
        for i in 1..(g.num_irqs() / 32) as u64 {
            w32(g, true, 0x80 + i * 4, !0); // all group 1
            w32(g, true, 0x180 + i * 4, !0); // disable
        }
        for i in 8..(g.num_irqs() / 4) as u64 {
            w32(g, true, 0x400 + i * 4, 0xa0a0a0a0);
        }
        w32(g, true, 0x0, 0b10 | (1 << 4));
        for c in 0..n as u64 {
            let rd = c * GICR_STRIDE;
            w32(g, false, rd + 0x14, 0); // wake
            w32(g, false, rd + SGI_BASE + 0x80, !0);
            for i in 0..8u64 {
                w32(g, false, rd + SGI_BASE + 0x400 + i * 4, 0xa0a0a0a0);
            }
            g.sysreg_write(c as usize, sysreg::ICC_PMR_EL1, 0xf0);
            g.sysreg_write(c as usize, sysreg::ICC_BPR1_EL1, 0);
            g.sysreg_write(c as usize, sysreg::ICC_CTLR_EL1, 0);
            g.sysreg_write(c as usize, sysreg::ICC_IGRPEN1_EL1, 1);
        }
    }

    #[test]
    fn identification_registers() {
        let g = GicV3::new(4, 64);
        assert_eq!(r32(&g, true, 0xffe8) >> 4 & 0xf, 3);
        assert_eq!(r32(&g, true, 0x4) & 0x1f, 2); // 96 INTIDs -> 3 banks
        let mut b = [0u8; 8];
        g.redist_read(3 * GICR_STRIDE + 8, &mut b);
        let typer = u64::from_le_bytes(b);
        assert_eq!(typer >> 32, 3);
        assert_ne!(typer & (1 << 4), 0, "last redistributor");
        g.redist_read(2 * GICR_STRIDE + 8, &mut b);
        assert_eq!(u64::from_le_bytes(b) & (1 << 4), 0);
        // WAKER: ChildrenAsleep follows ProcessorSleep
        assert_eq!(r32(&g, false, 0x14), 0b110);
        w32(&g, false, 0x14, 0);
        assert_eq!(r32(&g, false, 0x14), 0);
    }

    #[test]
    fn level_spi_ack_eoi_cycle() {
        let g = GicV3::new(2, 64);
        linux_init(&g, 2);
        let intid = 32 + 16;
        // route to CPU 1, enable, level triggered
        g.dist_write(0x6000 + intid as u64 * 8, &crate::cpu::mpidr_for(1).to_le_bytes());
        w32(&g, true, 0x100 + (intid / 32) as u64 * 4, 1 << (intid % 32));
        g.set_spi(intid, true);
        assert!(!g.irq_line(0));
        assert!(g.irq_line(1));
        assert_eq!(g.sysreg_read(1, sysreg::ICC_HPPIR1_EL1), Some(intid as u64));
        assert_eq!(g.sysreg_read(1, sysreg::ICC_IAR1_EL1), Some(intid as u64));
        assert!(!g.irq_line(1), "active interrupt must not re-signal");
        assert_eq!(g.sysreg_read(1, sysreg::ICC_RPR_EL1), Some(0xa0));
        // Device lowers the line before EOI (driver acked the device).
        g.set_spi(intid, false);
        g.sysreg_write(1, sysreg::ICC_EOIR1_EL1, intid as u64);
        assert!(!g.irq_line(1));
        assert_eq!(g.sysreg_read(1, sysreg::ICC_IAR1_EL1), Some(SPURIOUS as u64));
        assert_eq!(g.sysreg_read(1, sysreg::ICC_RPR_EL1), Some(0xff));
        // Level still high after EOI -> fires again.
        g.set_spi(intid, true);
        assert!(g.irq_line(1));
    }

    #[test]
    fn edge_timer_ppi_and_priority_mask() {
        let g = GicV3::new(1, 32);
        linux_init(&g, 1);
        w32(&g, false, SGI_BASE + 0x100, 1 << 27); // enable vtimer PPI (level)
        g.set_ppi_level(0, 27, true);
        assert!(g.irq_line(0));
        // Mask everything with PMR=0.
        g.sysreg_write(0, sysreg::ICC_PMR_EL1, 0);
        assert!(!g.irq_line(0));
        assert_eq!(g.sysreg_read(0, sysreg::ICC_IAR1_EL1), Some(SPURIOUS as u64));
        g.sysreg_write(0, sysreg::ICC_PMR_EL1, 0xf0);
        assert!(g.irq_line(0));
        assert_eq!(g.sysreg_read(0, sysreg::ICC_IAR1_EL1), Some(27));
        g.set_ppi_level(0, 27, false);
        g.sysreg_write(0, sysreg::ICC_EOIR1_EL1, 27);
        assert!(!g.irq_line(0));
    }

    #[test]
    fn sgi_targets_and_notifier() {
        let g = GicV3::new(4, 32);
        linux_init(&g, 4);
        let kicked = Arc::new(AtomicUsize::new(0));
        let k = kicked.clone();
        g.set_notifier(Box::new(move |cpu| {
            k.fetch_or(1 << cpu, Ordering::SeqCst);
        }));
        for c in 0..4u64 {
            w32(&g, false, c * GICR_STRIDE + SGI_BASE + 0x100, 0xffff); // enable SGIs
        }
        // SGI 5 to CPUs 1 and 3 (target list bits 1 and 3).
        g.sysreg_write(0, sysreg::ICC_SGI1R_EL1, (5 << 24) | 0b1010);
        assert_eq!(kicked.load(Ordering::SeqCst), 0b1010);
        assert_eq!(g.sysreg_read(3, sysreg::ICC_IAR1_EL1), Some(5));
        assert_eq!(g.sysreg_read(2, sysreg::ICC_IAR1_EL1), Some(SPURIOUS as u64));
        // Same-priority interrupts cannot preempt: finish SGI 5 on CPU 3 first.
        g.sysreg_write(3, sysreg::ICC_EOIR1_EL1, 5);
        // Broadcast (IRM) excludes the sender.
        kicked.store(0, Ordering::SeqCst);
        g.sysreg_write(2, sysreg::ICC_SGI1R_EL1, (1 << 40) | (1 << 24));
        assert_eq!(kicked.load(Ordering::SeqCst) & 0b0100, 0);
        assert!(g.irq_line(0) && g.irq_line(1) && g.irq_line(3));
    }

    #[test]
    fn eoimode1_needs_dir_and_preemption_works() {
        let g = GicV3::new(1, 32);
        linux_init(&g, 1);
        g.sysreg_write(0, sysreg::ICC_CTLR_EL1, 2); // EOImode=1
        w32(&g, false, SGI_BASE + 0x100, 0b11); // enable SGI 0 and 1
                                                // Give SGI 1 a higher priority than SGI 0.
        g.redist_write(SGI_BASE + 0x400, &[0xa0, 0x80]);
        w32(&g, false, SGI_BASE + 0x200, 0b01); // pend SGI0
        assert_eq!(g.sysreg_read(0, sysreg::ICC_IAR1_EL1), Some(0));
        w32(&g, false, SGI_BASE + 0x200, 0b10); // pend SGI1 -> preempts
        assert!(g.irq_line(0));
        assert_eq!(g.sysreg_read(0, sysreg::ICC_IAR1_EL1), Some(1));
        assert_eq!(g.sysreg_read(0, sysreg::ICC_AP1R0_EL1), Some((1 << (0xa0 >> 3)) | (1 << (0x80 >> 3))));
        g.sysreg_write(0, sysreg::ICC_EOIR1_EL1, 1);
        g.sysreg_write(0, sysreg::ICC_EOIR1_EL1, 0);
        // Priority dropped, but still active until DIR.
        assert_eq!(r32(&g, false, SGI_BASE + 0x300) & 0b11, 0b11);
        g.sysreg_write(0, sysreg::ICC_DIR_EL1, 0);
        g.sysreg_write(0, sysreg::ICC_DIR_EL1, 1);
        assert_eq!(r32(&g, false, SGI_BASE + 0x300) & 0b11, 0);
    }
}
