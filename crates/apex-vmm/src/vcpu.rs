//! vCPU execution: the loop that runs guest code and services every exit
//! (MMIO, system registers, WFI, PSCI, timer), written against the
//! `VirtualCpu` trait so it is exercised by unit tests with a scripted vCPU.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use apex_arm64::cpu::{index_for, mpidr_for, EntryState, PSTATE_EL1H_DAIF, SCTLR_EL1_RESET};
use apex_arm64::esr::{self, ec, DataAbort, SysRegAccess, Wfx};
use apex_arm64::gic::GicV3;
use apex_arm64::psci::{self, PsciOutcome, PsciPlatform};
use apex_arm64::sysreg::{self, cntctl};
use apex_core::bus::MmioBus;
use apex_core::hv::{GicMode, Hypervisor, Reg, VcpuExit, VirtualCpu};
use apex_core::irq::InterruptController;
use apex_core::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    PowerOff,
    Reset,
    Requested,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Power {
    Off,
    OnPending(EntryState),
    On,
}

struct SlotState {
    power: Power,
    kicked: bool,
}

struct CpuSlot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

/// Per-vCPU exit counters (diagnostics / `apex stats`).
#[derive(Default)]
pub struct ExitStats {
    pub mmio: AtomicU64,
    pub sysreg: AtomicU64,
    pub wfi: AtomicU64,
    pub psci: AtomicU64,
    pub vtimer: AtomicU64,
    pub canceled: AtomicU64,
}

/// Power/wake state of every vCPU. Created before devices so interrupt
/// controllers can wake sleeping vCPUs.
pub struct CpuSet {
    hv: Arc<dyn Hypervisor>,
    slots: Vec<CpuSlot>,
    stopping: AtomicBool,
    stop_reason: Mutex<Option<StopReason>>,
    stop_cv: Condvar,
    pub stats: Vec<ExitStats>,
}

impl CpuSet {
    pub fn new(hv: Arc<dyn Hypervisor>, n: usize) -> Arc<CpuSet> {
        Arc::new(CpuSet {
            hv,
            slots: (0..n)
                .map(|_| CpuSlot { state: Mutex::new(SlotState { power: Power::Off, kicked: false }), cv: Condvar::new() })
                .collect(),
            stopping: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
            stop_cv: Condvar::new(),
            stats: (0..n).map(|_| ExitStats::default()).collect(),
        })
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn hypervisor(&self) -> &Arc<dyn Hypervisor> {
        &self.hv
    }

    /// Wake vCPU `idx` from WFI and force it out of guest mode.
    pub fn kick(&self, idx: usize) {
        if let Some(s) = self.slots.get(idx) {
            let mut st = s.state.lock().unwrap();
            st.kicked = true;
            s.cv.notify_all();
        }
        self.hv.kick_vcpus(&[idx]);
    }

    /// Wake every vCPU sleeping in WFI without forcing running ones out.
    pub fn wake_all(&self) {
        for s in &self.slots {
            let mut st = s.state.lock().unwrap();
            st.kicked = true;
            s.cv.notify_all();
        }
    }

    pub fn kick_all(&self) {
        self.wake_all();
        let all: Vec<usize> = (0..self.slots.len()).collect();
        self.hv.kick_vcpus(&all);
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub fn request_stop(&self, reason: StopReason) {
        {
            let mut r = self.stop_reason.lock().unwrap();
            if r.is_none() {
                match &reason {
                    StopReason::Error(e) => apex_core::error!("VM stopping: {e}"),
                    other => apex_core::info!("VM stopping: {other:?}"),
                }
                *r = Some(reason);
            }
            self.stopping.store(true, Ordering::Release);
            self.stop_cv.notify_all();
        }
        self.kick_all();
    }

    pub fn wait_stopped(&self) -> StopReason {
        let mut r = self.stop_reason.lock().unwrap();
        while r.is_none() {
            r = self.stop_cv.wait(r).unwrap();
        }
        r.clone().unwrap()
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason.lock().unwrap().clone()
    }

    fn set_power(&self, idx: usize, p: Power) {
        self.slots[idx].state.lock().unwrap().power = p;
    }

    /// Block until CPU_ON targets this CPU (or the VM stops).
    fn wait_power_on(&self, idx: usize) -> Option<EntryState> {
        let s = &self.slots[idx];
        let mut st = s.state.lock().unwrap();
        loop {
            if self.is_stopping() {
                return None;
            }
            if let Power::OnPending(e) = st.power {
                return Some(e);
            }
            st = s.cv.wait(st).unwrap();
        }
    }

    /// Sleep until kicked or `timeout` elapses. `ready` is evaluated under
    /// the slot lock to avoid lost wake-ups.
    fn wait_kick(&self, idx: usize, timeout: Duration, ready: impl Fn() -> bool) {
        let s = &self.slots[idx];
        let mut st = s.state.lock().unwrap();
        if st.kicked || ready() || self.is_stopping() {
            st.kicked = false;
            return;
        }
        let (mut st, _) = s.cv.wait_timeout(st, timeout).unwrap();
        st.kicked = false;
    }
}

impl PsciPlatform for CpuSet {
    fn cpu_on(&self, mpidr: u64, entry: u64, context: u64) -> i64 {
        let Some(idx) = index_for(mpidr, self.slots.len()) else { return psci::ret::INVALID_PARAMETERS };
        let s = &self.slots[idx];
        let mut st = s.state.lock().unwrap();
        match st.power {
            Power::On => psci::ret::ALREADY_ON,
            Power::OnPending(_) => psci::ret::ON_PENDING,
            Power::Off => {
                st.power = Power::OnPending(EntryState { pc: entry, x0: context });
                s.cv.notify_all();
                psci::ret::SUCCESS
            }
        }
    }

    fn affinity_info(&self, mpidr: u64) -> i64 {
        let Some(idx) = index_for(mpidr, self.slots.len()) else { return psci::ret::INVALID_PARAMETERS };
        match self.slots[idx].state.lock().unwrap().power {
            Power::On => psci::AFF_ON,
            Power::OnPending(_) => psci::AFF_ON_PENDING,
            Power::Off => psci::AFF_OFF,
        }
    }

    fn fill_random(&self, buf: &mut [u8]) -> bool {
        apex_core::sys::fill_random(buf).is_ok()
    }
}

/// Interrupt controller facade for the in-kernel vGIC.
pub struct HwIrqChip {
    cpus: Arc<CpuSet>,
    spi_base: u32,
    spi_count: u32,
}

impl HwIrqChip {
    pub fn new(cpus: Arc<CpuSet>) -> HwIrqChip {
        let g = cpus.hv.gic_geometry();
        HwIrqChip { cpus, spi_base: g.spi_base, spi_count: g.spi_count }
    }
}

impl InterruptController for HwIrqChip {
    fn set_spi_level(&self, spi: u32, level: bool) {
        if let Err(e) = self.cpus.hv.hw_set_spi(self.spi_base + spi, level) {
            apex_core::warn!("SPI {spi}: {e}");
        }
        if level {
            // The host kernel delivers the interrupt; only WFI sleepers in
            // our own loop need a nudge.
            self.cpus.wake_all();
        }
    }
    fn spi_count(&self) -> u32 {
        self.spi_count
    }
}

/// Shared by all vCPU threads.
pub struct VcpuHub {
    pub cpus: Arc<CpuSet>,
    pub bus: Arc<MmioBus>,
    /// Userspace GIC when not using the in-kernel vGIC.
    pub gic: Option<Arc<GicV3>>,
    pub vtimer_ppi: u32,
    warned: Mutex<HashSet<u64>>,
}

impl VcpuHub {
    pub fn new(cpus: Arc<CpuSet>, bus: Arc<MmioBus>, gic: Option<Arc<GicV3>>) -> Arc<VcpuHub> {
        let vtimer_ppi = cpus.hv.gic_geometry().vtimer_ppi;
        Arc::new(VcpuHub { cpus, bus, gic, vtimer_ppi, warned: Mutex::new(HashSet::new()) })
    }

    fn warn_once(&self, key: u64, msg: impl FnOnce() -> String) {
        if self.warned.lock().unwrap().insert(key) {
            apex_core::warn!("{}", msg());
        }
    }
}

enum Flow {
    Continue,
    CpuOff,
    Stop,
}

fn get_x(v: &dyn VirtualCpu, n: u8) -> Result<u64> {
    if n == 31 {
        Ok(0)
    } else {
        v.get_reg(Reg::X(n))
    }
}

fn set_x(v: &mut dyn VirtualCpu, n: u8, val: u64) -> Result<()> {
    if n == 31 {
        Ok(())
    } else {
        v.set_reg(Reg::X(n), val)
    }
}

fn advance_pc(v: &mut dyn VirtualCpu) -> Result<()> {
    let pc = v.get_reg(Reg::Pc)?;
    v.set_reg(Reg::Pc, pc.wrapping_add(4))
}

fn reset_vcpu(v: &mut dyn VirtualCpu, entry: EntryState) -> Result<()> {
    for n in 1..=30u8 {
        v.set_reg(Reg::X(n), 0)?;
    }
    v.set_reg(Reg::X(0), entry.x0)?;
    v.set_reg(Reg::Pc, entry.pc)?;
    v.set_reg(Reg::Cpsr, PSTATE_EL1H_DAIF)?;
    v.set_sys_reg(sysreg::SCTLR_EL1.0, SCTLR_EL1_RESET)?;
    Ok(())
}

fn dump_state(v: &dyn VirtualCpu) -> String {
    let pc = v.get_reg(Reg::Pc).unwrap_or(0);
    let cpsr = v.get_reg(Reg::Cpsr).unwrap_or(0);
    let lr = v.get_reg(Reg::X(30)).unwrap_or(0);
    let elr = v.get_sys_reg(sysreg::ELR_EL1.0).unwrap_or(0);
    let esr1 = v.get_sys_reg(sysreg::ESR_EL1.0).unwrap_or(0);
    let far = v.get_sys_reg(sysreg::FAR_EL1.0).unwrap_or(0);
    format!("pc={pc:#x} lr={lr:#x} cpsr={cpsr:#x} elr_el1={elr:#x} esr_el1={esr1:#x} far_el1={far:#x}")
}

/// Released once every vCPU exists, so no guest code runs while the
/// in-kernel vGIC is still gaining redistributors.
#[derive(Default)]
pub struct StartGate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl StartGate {
    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }

    fn wait(&self, cpus: &CpuSet) {
        let mut o = self.open.lock().unwrap();
        while !*o && !cpus.is_stopping() {
            o = self.cv.wait_timeout(o, Duration::from_millis(50)).unwrap().0;
        }
    }
}

/// Start-up handshake with the machine: report creation, then wait for
/// the gate.
#[derive(Default)]
pub struct VcpuStart {
    pub created: Option<std::sync::mpsc::Sender<bool>>,
    pub gate: Option<Arc<StartGate>>,
}

/// Thread body for vCPU `idx`. CPU 0 starts at `boot`; the others wait for
/// PSCI CPU_ON.
pub fn vcpu_thread(hub: Arc<VcpuHub>, idx: usize, boot: Option<EntryState>) {
    vcpu_thread_with(hub, idx, boot, VcpuStart::default())
}

/// Like [`vcpu_thread`], with creation ordering. Hypervisor.framework
/// assigns vGIC redistributors in vCPU creation order, so the machine
/// creates vCPUs strictly by index before any of them runs.
pub fn vcpu_thread_with(hub: Arc<VcpuHub>, idx: usize, boot: Option<EntryState>, start: VcpuStart) {
    let cpus = hub.cpus.clone();
    let created = cpus.hv.create_vcpu(idx, mpidr_for(idx));
    if let Some(tx) = &start.created {
        let _ = tx.send(created.is_ok());
    }
    let mut vcpu = match created {
        Ok(v) => v,
        Err(e) => {
            cpus.request_stop(StopReason::Error(format!("vCPU {idx}: {e}")));
            return;
        }
    };
    if let Some(g) = &start.gate {
        g.wait(&cpus);
    }
    let mut pending = boot;
    loop {
        let entry = match pending.take() {
            Some(e) => e,
            None => match cpus.wait_power_on(idx) {
                Some(e) => e,
                None => break,
            },
        };
        cpus.set_power(idx, Power::On);
        apex_core::debug!("vCPU {idx} starting at {:#x}", entry.pc);
        let r = reset_vcpu(vcpu.as_mut(), entry).and_then(|_| run_loop(&hub, idx, vcpu.as_mut()));
        match r {
            Ok(Flow::CpuOff) => {
                cpus.set_power(idx, Power::Off);
                apex_core::debug!("vCPU {idx} powered off");
            }
            Ok(_) => break,
            Err(e) => {
                cpus.request_stop(StopReason::Error(format!("vCPU {idx}: {e} [{}]", dump_state(vcpu.as_ref()))));
                break;
            }
        }
    }
    cpus.set_power(idx, Power::Off);
}

fn run_loop(hub: &VcpuHub, idx: usize, vcpu: &mut dyn VirtualCpu) -> Result<Flow> {
    let cpus = &hub.cpus;
    let stats = &cpus.stats[idx];
    let mut vtimer_masked = false;
    let vtimer_offset = vcpu.vtimer_offset()?;
    loop {
        if cpus.is_stopping() {
            return Ok(Flow::Stop);
        }
        if let Some(gic) = &hub.gic {
            if vtimer_masked {
                let ctl = vcpu.get_sys_reg(sysreg::CNTV_CTL_EL0.0)?;
                if !cntctl::irq_asserted(ctl) {
                    gic.set_ppi_level(idx, hub.vtimer_ppi, false);
                    vcpu.set_vtimer_mask(false)?;
                    vtimer_masked = false;
                }
            }
            vcpu.set_irq_line(gic.irq_line(idx))?;
        }
        match vcpu.run()? {
            VcpuExit::Canceled => {
                stats.canceled.fetch_add(1, Ordering::Relaxed);
            }
            VcpuExit::VtimerActivated => {
                stats.vtimer.fetch_add(1, Ordering::Relaxed);
                match &hub.gic {
                    Some(gic) => {
                        gic.set_ppi_level(idx, hub.vtimer_ppi, true);
                        vtimer_masked = true;
                    }
                    None => vcpu.set_vtimer_mask(false)?,
                }
            }
            VcpuExit::Exception { syndrome, physical_address, .. } => {
                match handle_exception(hub, idx, vcpu, syndrome, physical_address, vtimer_offset)? {
                    Flow::Continue => {}
                    other => return Ok(other),
                }
            }
            VcpuExit::Unknown => return Err(Error::Hypervisor("unknown vCPU exit".into())),
        }
    }
}

fn handle_exception(hub: &VcpuHub, idx: usize, vcpu: &mut dyn VirtualCpu, esr_val: u64, ipa: u64, vtimer_offset: u64) -> Result<Flow> {
    let stats = &hub.cpus.stats[idx];
    match esr::exception_class(esr_val) {
        ec::DABT_LOWER => {
            stats.mmio.fetch_add(1, Ordering::Relaxed);
            let da = DataAbort::decode(esr_val);
            if !da.isv || da.s1ptw {
                return Err(Error::Hypervisor(format!("data abort without instruction syndrome at IPA {ipa:#x} (esr {esr_val:#x})")));
            }
            if da.write {
                let v = get_x(vcpu, da.srt)?;
                if !hub.bus.write(ipa, &v.to_le_bytes()[..da.size]) {
                    hub.warn_once(ipa & !0xfff, || format!("write to unmapped MMIO {ipa:#x} ignored"));
                }
            } else {
                let mut b = [0u8; 8];
                if !hub.bus.read(ipa, &mut b[..da.size]) {
                    hub.warn_once(ipa & !0xfff, || format!("read from unmapped MMIO {ipa:#x} returns 0"));
                }
                set_x(vcpu, da.srt, da.load_value(u64::from_le_bytes(b)))?;
            }
            advance_pc(vcpu)?;
        }
        ec::SYSREG => {
            stats.sysreg.fetch_add(1, Ordering::Relaxed);
            let acc = SysRegAccess::decode(esr_val);
            let reg = acc.id();
            if acc.read {
                let v = match hub.gic.as_ref().and_then(|g| g.sysreg_read(idx, reg)) {
                    Some(v) => v,
                    None => emulate_sysreg_read(hub, reg),
                };
                set_x(vcpu, acc.rt, v)?;
            } else {
                let v = get_x(vcpu, acc.rt)?;
                let handled = hub.gic.as_ref().map(|g| g.sysreg_write(idx, reg, v)).unwrap_or(false);
                if !handled {
                    emulate_sysreg_write(hub, reg, v);
                }
            }
            advance_pc(vcpu)?;
        }
        ec::WFX => {
            advance_pc(vcpu)?;
            match Wfx::decode(esr_val) {
                Wfx::Wfi | Wfx::Wfit => {
                    stats.wfi.fetch_add(1, Ordering::Relaxed);
                    wait_for_interrupt(hub, idx, vcpu, vtimer_offset)?;
                }
                _ => std::thread::yield_now(),
            }
        }
        ec::HVC64 | ec::SMC64 => {
            stats.psci.fetch_add(1, Ordering::Relaxed);
            let is_smc = esr::exception_class(esr_val) == ec::SMC64;
            let x = [get_x(vcpu, 0)?, get_x(vcpu, 1)?, get_x(vcpu, 2)?, get_x(vcpu, 3)?];
            let outcome = psci::handle(x, hub.cpus.as_ref());
            // HVC returns to the next instruction by hardware; SMC traps
            // before it and must be stepped over.
            if is_smc {
                advance_pc(vcpu)?;
            }
            match outcome {
                PsciOutcome::Return(r) => {
                    for (i, v) in r.iter().enumerate() {
                        vcpu.set_reg(Reg::X(i as u8), *v)?;
                    }
                }
                PsciOutcome::Suspend => {
                    vcpu.set_reg(Reg::X(0), 0)?;
                    wait_for_interrupt(hub, idx, vcpu, vtimer_offset)?;
                }
                PsciOutcome::CpuOff => return Ok(Flow::CpuOff),
                PsciOutcome::SystemOff => {
                    hub.cpus.request_stop(StopReason::PowerOff);
                    return Ok(Flow::Stop);
                }
                PsciOutcome::SystemReset => {
                    hub.cpus.request_stop(StopReason::Reset);
                    return Ok(Flow::Stop);
                }
            }
        }
        other => {
            return Err(Error::Hypervisor(format!(
                "unhandled exception class {other:#x} ({}) esr={esr_val:#x} ipa={ipa:#x}",
                ec::name(other)
            )));
        }
    }
    Ok(Flow::Continue)
}

/// Registers that trap but carry no state for a guest: debug, OS lock, PMU
/// (no PMU is advertised) and the physical timer (the guest uses CNTV).
fn emulate_sysreg_read(hub: &VcpuHub, reg: sysreg::SysReg) -> u64 {
    match reg {
        sysreg::OSLSR_EL1 => 0b1000, // OSLM[1] = 1: OS lock implemented, unlocked
        sysreg::CNTFRQ_EL0 => hub.cpus.hv.counter_frequency(),
        r if r.is_debug() || r.is_pmu() => 0,
        sysreg::CNTP_CTL_EL0 | sysreg::CNTP_CVAL_EL0 | sysreg::CNTP_TVAL_EL0 => 0,
        r => {
            hub.warn_once(0x1_0000_0000 | r.0 as u64, || format!("unhandled MRS {r:?}, reading as zero"));
            0
        }
    }
}

fn emulate_sysreg_write(hub: &VcpuHub, reg: sysreg::SysReg, v: u64) {
    if reg.is_debug() || reg.is_pmu() || matches!(reg, sysreg::CNTP_CTL_EL0 | sysreg::CNTP_CVAL_EL0 | sysreg::CNTP_TVAL_EL0) {
        return;
    }
    hub.warn_once(0x2_0000_0000 | reg.0 as u64, || format!("unhandled MSR {reg:?} <- {v:#x}, ignored"));
}

fn wait_for_interrupt(hub: &VcpuHub, idx: usize, vcpu: &mut dyn VirtualCpu, vtimer_offset: u64) -> Result<()> {
    let hv = hub.cpus.hypervisor();
    let ctl = vcpu.get_sys_reg(sysreg::CNTV_CTL_EL0.0)?;
    let mut timeout = Duration::from_millis(100);
    if cntctl::armed(ctl) {
        let cval = vcpu.get_sys_reg(sysreg::CNTV_CVAL_EL0.0)?;
        let now = hv.host_counter().wrapping_sub(vtimer_offset);
        if cval <= now {
            return Ok(()); // timer already due: re-enter and take it
        }
        let ticks = cval - now;
        let ns = (ticks as u128 * 1_000_000_000 / hv.counter_frequency().max(1) as u128).min(u64::MAX as u128) as u64;
        timeout = timeout.min(Duration::from_nanos(ns));
    }
    if hv.gic_mode() == GicMode::Hardware {
        // Interrupts may become pending inside the host kernel without us
        // seeing them (e.g. IPIs): never sleep long in userspace.
        timeout = timeout.min(Duration::from_micros(500));
    }
    let gic = hub.gic.clone();
    hub.cpus.wait_kick(idx, timeout, || gic.as_ref().map(|g| g.irq_line(idx)).unwrap_or(false));
    Ok(())
}

#[cfg(test)]
pub(crate) mod mock {
    //! A scripted hypervisor used to test the exit handling end to end.

    use super::*;
    use apex_core::hv::{GicGeometry, MemFlags};
    use std::collections::{HashMap, VecDeque};

    pub type Step = Box<dyn FnMut(&mut MockCpu) -> VcpuExit + Send>;
    pub type Script = Vec<Step>;

    pub struct MockCpu {
        pub idx: usize,
        pub regs: HashMap<u32, u64>,
        pub sys: HashMap<u16, u64>,
        pub irq_line: bool,
        pub script: VecDeque<Step>,
    }

    impl VirtualCpu for MockCpu {
        fn index(&self) -> usize {
            self.idx
        }
        fn run(&mut self) -> Result<VcpuExit> {
            match self.script.pop_front() {
                Some(mut f) => Ok(f(self)),
                None => {
                    // Script exhausted: power the system off via PSCI.
                    self.regs.insert(0, psci::fid::SYSTEM_OFF as u64);
                    Ok(exit_esr(ec::HVC64, 0))
                }
            }
        }
        fn get_reg(&self, reg: Reg) -> Result<u64> {
            Ok(*self.regs.get(&reg.hvf_id()).unwrap_or(&0))
        }
        fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
            self.regs.insert(reg.hvf_id(), value);
            Ok(())
        }
        fn get_sys_reg(&self, reg: u16) -> Result<u64> {
            Ok(*self.sys.get(&reg).unwrap_or(&0))
        }
        fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<()> {
            self.sys.insert(reg, value);
            Ok(())
        }
        fn set_irq_line(&mut self, asserted: bool) -> Result<()> {
            self.irq_line = asserted;
            Ok(())
        }
        fn set_vtimer_mask(&mut self, _masked: bool) -> Result<()> {
            Ok(())
        }
        fn vtimer_offset(&self) -> Result<u64> {
            Ok(0)
        }
    }

    pub fn exit_esr(class: u32, iss: u32) -> VcpuExit {
        VcpuExit::Exception { syndrome: ((class as u64) << 26) | (1 << 25) | iss as u64, virtual_address: 0, physical_address: 0 }
    }

    pub fn mmio_exit(ipa: u64, write: bool, size_log2: u32, reg: u32) -> VcpuExit {
        let iss = (1 << 24) | (size_log2 << 22) | (reg << 16) | ((write as u32) << 6) | (1 << 15);
        match exit_esr(ec::DABT_LOWER, iss) {
            VcpuExit::Exception { syndrome, .. } => VcpuExit::Exception { syndrome, virtual_address: 0, physical_address: ipa },
            e => e,
        }
    }

    pub struct MockHv {
        pub scripts: Mutex<HashMap<usize, Script>>,
    }

    impl Hypervisor for MockHv {
        fn name(&self) -> &str {
            "mock"
        }
        unsafe fn map_memory(&self, _h: *mut u8, _g: u64, _s: u64, _f: MemFlags) -> Result<()> {
            Ok(())
        }
        fn unmap_memory(&self, _g: u64, _s: u64) -> Result<()> {
            Ok(())
        }
        fn create_vcpu(&self, index: usize, _mpidr: u64) -> Result<Box<dyn VirtualCpu>> {
            let script = self.scripts.lock().unwrap().remove(&index).unwrap_or_default();
            Ok(Box::new(MockCpu {
                idx: index,
                regs: HashMap::new(),
                sys: HashMap::new(),
                irq_line: false,
                script: script.into_iter().collect(),
            }))
        }
        fn kick_vcpus(&self, _indices: &[usize]) {}
        fn gic_mode(&self) -> GicMode {
            GicMode::Emulated
        }
        fn gic_geometry(&self) -> GicGeometry {
            GicGeometry::default()
        }
        fn hw_set_spi(&self, _intid: u32, _level: bool) -> Result<()> {
            Ok(())
        }
        fn counter_frequency(&self) -> u64 {
            24_000_000
        }
        fn host_counter(&self) -> u64 {
            apex_core::sys::host_ticks()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::*;
    use super::*;
    use apex_core::bus::MmioDevice;
    use std::collections::HashMap;

    struct Scratch(Mutex<u64>);
    impl MmioDevice for Scratch {
        fn read(&self, _o: u64, d: &mut [u8]) {
            let v = *self.0.lock().unwrap();
            d.copy_from_slice(&v.to_le_bytes()[..d.len()]);
        }
        fn write(&self, _o: u64, d: &[u8]) {
            let mut b = [0u8; 8];
            b[..d.len()].copy_from_slice(d);
            *self.0.lock().unwrap() = u64::from_le_bytes(b);
        }
    }

    fn setup(scripts: HashMap<usize, Script>, ncpu: usize) -> (Arc<VcpuHub>, Arc<Scratch>) {
        let hv: Arc<dyn Hypervisor> = Arc::new(MockHv { scripts: Mutex::new(scripts) });
        let cpus = CpuSet::new(hv, ncpu);
        let scratch = Arc::new(Scratch(Mutex::new(0)));
        let mut bus = MmioBus::new();
        bus.insert(0x1000, 0x100, scratch.clone()).unwrap();
        let gic = GicV3::new(ncpu, 64);
        let c2 = cpus.clone();
        gic.set_notifier(Box::new(move |c| c2.kick(c)));
        bus.insert(0x0800_0000, 0x1_0000, Arc::new(apex_arm64::gic::Distributor(gic.clone()))).unwrap();
        (VcpuHub::new(cpus, Arc::new(bus), Some(gic)), scratch)
    }

    #[test]
    fn mmio_roundtrip_and_poweroff() {
        let mut scripts: HashMap<usize, Script> = HashMap::new();
        scripts.insert(
            0,
            vec![
                Box::new(|c: &mut MockCpu| {
                    c.regs.insert(3, 0xdead_beef);
                    mmio_exit(0x1000, true, 2, 3) // str w3, [scratch]
                }),
                Box::new(|_c: &mut MockCpu| mmio_exit(0x1000, false, 1, 5)), // ldrh x5
                Box::new(|c: &mut MockCpu| {
                    assert_eq!(c.regs[&5], 0xbeef);
                    assert_eq!(c.regs[&Reg::Pc.hvf_id()], 0x4000_0008, "pc advanced twice");
                    mmio_exit(0x9999_0000, false, 2, 6) // unmapped: reads zero
                }),
            ],
        );
        let (hub, scratch) = setup(scripts, 1);
        vcpu_thread(hub.clone(), 0, Some(EntryState { pc: 0x4000_0000, x0: 0x8000_0000 }));
        assert_eq!(*scratch.0.lock().unwrap(), 0xdead_beef);
        assert_eq!(hub.cpus.wait_stopped(), StopReason::PowerOff);
        assert_eq!(hub.cpus.stats[0].mmio.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn psci_cpu_on_starts_secondary() {
        let mut scripts: HashMap<usize, Script> = HashMap::new();
        scripts.insert(
            0,
            vec![
                Box::new(|c: &mut MockCpu| {
                    c.regs.insert(0, 0xc400_0003); // CPU_ON (SMC64 id)
                    c.regs.insert(1, 1); // MPIDR of CPU 1
                    c.regs.insert(2, 0x4100_0000);
                    c.regs.insert(3, 0x1234);
                    exit_esr(ec::HVC64, 0)
                }),
                Box::new(|c: &mut MockCpu| {
                    assert_eq!(c.regs[&0], 0, "CPU_ON returns SUCCESS");
                    // spin until CPU 1 reports in via MMIO
                    std::thread::sleep(Duration::from_millis(50));
                    VcpuExit::Canceled
                }),
            ],
        );
        scripts.insert(
            1,
            vec![Box::new(|c: &mut MockCpu| {
                assert_eq!(c.regs[&Reg::Pc.hvf_id()], 0x4100_0000);
                assert_eq!(c.regs[&0], 0x1234, "context id in x0");
                c.regs.insert(7, 77);
                mmio_exit(0x1000, true, 3, 7)
            })],
        );
        let (hub, scratch) = setup(scripts, 2);
        let h1 = hub.clone();
        let t1 = std::thread::spawn(move || vcpu_thread(h1, 1, None));
        vcpu_thread(hub.clone(), 0, Some(EntryState { pc: 0x4000_0000, x0: 0 }));
        t1.join().unwrap();
        assert_eq!(*scratch.0.lock().unwrap(), 77);
        assert_eq!(hub.cpus.wait_stopped(), StopReason::PowerOff);
    }

    #[test]
    fn gic_sysregs_and_timer_injection() {
        let mut scripts: HashMap<usize, Script> = HashMap::new();
        // Enable the distributor + CPU interface through traps, then take a
        // virtual timer interrupt through ICC_IAR1_EL1.
        let icc = |reg: sysreg::SysReg, rt: u32, read: bool| {
            let iss = ((reg.op0() as u32) << 20)
                | ((reg.op2() as u32) << 17)
                | ((reg.op1() as u32) << 14)
                | ((reg.crn() as u32) << 10)
                | (rt << 5)
                | ((reg.crm() as u32) << 1)
                | read as u32;
            exit_esr(ec::SYSREG, iss)
        };
        scripts.insert(
            0,
            vec![
                Box::new(|c: &mut MockCpu| {
                    c.regs.insert(1, 0b10);
                    mmio_exit(0x0800_0000, true, 2, 1) // GICD_CTLR.EnableGrp1
                }),
                Box::new(move |c: &mut MockCpu| {
                    c.regs.insert(2, 0xf0);
                    icc(sysreg::ICC_PMR_EL1, 2, false)
                }),
                Box::new(move |c: &mut MockCpu| {
                    c.regs.insert(2, 1);
                    icc(sysreg::ICC_IGRPEN1_EL1, 2, false)
                }),
                Box::new(|c: &mut MockCpu| {
                    // Vtimer PPI 27 is private to the redistributor; enable it
                    // directly on the model (no GICR mapped in this test).
                    let _ = c;
                    VcpuExit::Canceled
                }),
            ],
        );
        let (hub, _) = setup(scripts, 1);
        let gic = hub.gic.clone().unwrap();
        gic.redist_write(0x1_0000 + 0x80, &(1u32 << 27).to_le_bytes());
        gic.redist_write(0x1_0000 + 0x100, &(1u32 << 27).to_le_bytes());
        gic.redist_write(0x14, &0u32.to_le_bytes());
        let hub2 = hub.clone();
        vcpu_thread(hub2, 0, Some(EntryState { pc: 0, x0: 0 }));
        // After the script, deliver a timer tick and read IAR through the model.
        gic.set_ppi_level(0, 27, true);
        assert!(gic.irq_line(0));
        assert_eq!(gic.sysreg_read(0, sysreg::ICC_IAR1_EL1), Some(27));
    }

    #[test]
    fn unknown_exception_stops_with_error() {
        let mut scripts: HashMap<usize, Script> = HashMap::new();
        scripts.insert(0, vec![Box::new(|_c: &mut MockCpu| exit_esr(ec::IABT_LOWER, 0x10))]);
        let (hub, _) = setup(scripts, 1);
        vcpu_thread(hub.clone(), 0, Some(EntryState { pc: 0x1000, x0: 0 }));
        match hub.cpus.wait_stopped() {
            StopReason::Error(e) => assert!(e.contains("iabt-lower"), "{e}"),
            other => panic!("{other:?}"),
        }
    }
}
