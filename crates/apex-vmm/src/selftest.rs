//! `apex selftest`: boots a tiny bare-metal guest (guest/selftest/selftest.S)
//! through the normal machine path and checks that MMIO, the GIC, the
//! virtual timer interrupt, SMP bring-up and PSCI all work on this host.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apex_core::hv::GicRequest;
use apex_core::{Error, Result, MIB};

use crate::config::{SerialSink, VmConfig};
use crate::machine::{HostHooks, Machine};
use crate::vcpu::StopReason;

/// Assembled guest (scripts/gen-selftest.sh).
pub const IMAGE: &[u8] = include_bytes!("../../../guest/selftest/selftest.bin");
pub const EXPECTED: &str = "APEX selftest: MGT2\n";

#[derive(Debug)]
pub struct Outcome {
    pub gic: GicRequest,
    pub console: String,
    pub stop: StopReason,
    pub elapsed: Duration,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.stop == StopReason::PowerOff && self.console.contains(EXPECTED)
    }
}

/// Run the self test with the given interrupt controller. Errors mean the
/// VM could not be created at all (e.g. no hypervisor access).
pub fn run(gic: GicRequest, timeout: Duration) -> Result<Outcome> {
    let dir = std::env::temp_dir().join(format!("apex-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let kernel: PathBuf = dir.join("selftest.img");
    std::fs::write(&kernel, IMAGE)?;

    let mut cfg = VmConfig::for_kernel(kernel, None, "");
    cfg.cpus = 2;
    cfg.memory = 256 * MIB;
    cfg.gic = gic;
    cfg.serial = SerialSink::Callback;
    cfg.keyboard = false;

    let console = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = console.clone();
    let hooks = HostHooks { serial: Some(Arc::new(move |b: &[u8]| sink.lock().unwrap().extend_from_slice(b))), net_tx: None };

    let started = Instant::now();
    let machine = Arc::new(Machine::build(cfg, hooks)?);
    machine.start()?;

    // Watchdog: a hung guest must not hang the test.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let wd = {
        let m = machine.clone();
        std::thread::spawn(move || {
            if done_rx.recv_timeout(timeout).is_err() {
                m.cpus().request_stop(StopReason::Error(format!("selftest timed out after {timeout:?}")));
            }
        })
    };
    let stop = machine.wait();
    let _ = done_tx.send(());
    let _ = wd.join();
    let elapsed = started.elapsed();
    drop(machine);
    let _ = std::fs::remove_dir_all(&dir);

    let out = String::from_utf8_lossy(&console.lock().unwrap()).into_owned();
    Ok(Outcome { gic, console: out, stop, elapsed })
}

/// Whether an error from `run` means "this host cannot run VMs" rather
/// than a VMM bug.
pub fn is_unavailable(e: &Error) -> bool {
    match e {
        Error::Unsupported(_) => true,
        Error::Hypervisor(m) => m.contains("HV_DENIED") || m.contains("HV_UNSUPPORTED") || m.contains("HV_NO_DEVICE"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_arm64::boot::image::ImageHeader;

    #[test]
    fn embedded_image_is_a_valid_arm64_image() {
        let h = ImageHeader::parse(IMAGE).unwrap();
        assert_eq!(h.text_offset, 0);
        assert!(h.image_size as usize >= IMAGE.len());
        // Vector table at +0x800 with the IRQ entry at +0x280 being a branch.
        let irq = u32::from_le_bytes(IMAGE[0xa80..0xa84].try_into().unwrap());
        assert_eq!(irq >> 26, 0b000101, "unconditional B at the IRQ vector");
        // First instruction branches over the 64-byte header.
        let b = u32::from_le_bytes(IMAGE[0..4].try_into().unwrap());
        assert_eq!(b, 0x1400_0010);
    }

    #[test]
    fn unavailable_hosts_are_recognised() {
        assert!(is_unavailable(&Error::Unsupported("x".into())));
        assert!(is_unavailable(&Error::Hypervisor("hv_vm_create: HV_DENIED (missing entitlement)".into())));
        assert!(!is_unavailable(&Error::Boot("x".into())));
    }

    #[test]
    fn non_mac_hosts_report_unavailable() {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return;
        }
        let e = run(GicRequest::Emulated, Duration::from_secs(1)).unwrap_err();
        assert!(is_unavailable(&e), "{e}");
    }
}
