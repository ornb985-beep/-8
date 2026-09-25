//! C ABI consumed by the macOS frontend (`include/apex.h`).
//!
//! Every entry point is panic-safe (a panic becomes an error code) and
//! tolerates NULL handles. Safety contracts for the raw pointers are
//! documented once, in `include/apex.h`.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};

use apex_core::log::{self, Level};
use apex_devices::battery::{BatteryState, ChargeStatus};
use apex_devices::display::FrameInfo;
use apex_devices::input::{mac_keycode_to_linux, Contact};

use crate::config::VmConfig;
use crate::machine::{HostHooks, Machine};
use crate::vcpu::StopReason;

pub type ApexLogFn = Option<extern "C" fn(ctx: *mut c_void, level: i32, msg: *const c_char)>;
pub type ApexBytesFn = Option<extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize)>;

#[repr(C)]
pub struct ApexHooks {
    pub serial: ApexBytesFn,
    pub serial_ctx: *mut c_void,
    pub net_tx: ApexBytesFn,
    pub net_ctx: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ApexTouch {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub pressure: i32,
    pub major: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ApexDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub dpi: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ApexStats {
    pub frames_submitted: u64,
    pub frames_presented: u64,
    pub vsyncs: u64,
    pub exits_mmio: u64,
    pub exits_sysreg: u64,
    pub exits_wfi: u64,
    pub exits_psci: u64,
    pub exits_vtimer: u64,
}

pub const APEX_OK: i32 = 0;
pub const APEX_ERR: i32 = -1;
pub const APEX_ERR_PANIC: i32 = -2;

pub const APEX_STOP_NONE: i32 = 0;
pub const APEX_STOP_POWEROFF: i32 = 1;
pub const APEX_STOP_RESET: i32 = 2;
pub const APEX_STOP_REQUESTED: i32 = 3;
pub const APEX_STOP_ERROR: i32 = 4;

/// Raw pointer made shareable across threads; the C side guarantees the
/// context stays valid while the VM exists.
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

pub struct ApexVm {
    machine: Machine,
    last_error: Mutex<Option<CString>>,
}

fn write_err(buf: *mut c_char, len: usize, msg: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(len - 1);
    // SAFETY: caller provides a writable buffer of `len` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
}

fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        apex_core::error!("panic caught at the C ABI boundary");
        default
    })
}

unsafe fn vm<'a>(p: *mut ApexVm) -> Option<&'a ApexVm> {
    p.as_ref()
}

fn stop_code(r: &StopReason) -> i32 {
    match r {
        StopReason::PowerOff => APEX_STOP_POWEROFF,
        StopReason::Reset => APEX_STOP_RESET,
        StopReason::Requested => APEX_STOP_REQUESTED,
        StopReason::Error(_) => APEX_STOP_ERROR,
    }
}

#[no_mangle]
pub extern "C" fn apex_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Route VMM logs to the host application.
#[no_mangle]
pub extern "C" fn apex_set_log(f: ApexLogFn, ctx: *mut c_void, max_level: i32) {
    guard((), || {
        let lvl = match max_level {
            ..=1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            _ => Level::Trace,
        };
        log::set_max_level(lvl);
        match f {
            Some(cb) => {
                let c = Ctx(ctx);
                log::set_sink(Some(Box::new(move |l, m| {
                    if let Ok(s) = CString::new(m.replace('\0', " ")) {
                        let c = c;
                        cb(c.0, l as i32, s.as_ptr());
                    }
                })));
            }
            None => log::set_sink(None),
        }
    })
}

/// Host capabilities string; free with `apex_string_free`.
#[no_mangle]
pub extern "C" fn apex_host_capabilities() -> *mut c_char {
    guard(std::ptr::null_mut(), || CString::new(apex_hvf::host_capabilities()).map(CString::into_raw).unwrap_or(std::ptr::null_mut()))
}

#[no_mangle]
pub unsafe extern "C" fn apex_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

fn hooks_from(h: *const ApexHooks) -> HostHooks {
    let mut hooks = HostHooks::default();
    // SAFETY: NULL or a valid struct supplied by the caller.
    if let Some(h) = unsafe { h.as_ref() } {
        if let Some(cb) = h.serial {
            let c = Ctx(h.serial_ctx);
            hooks.serial = Some(Arc::new(move |b: &[u8]| {
                let c = c;
                cb(c.0, b.as_ptr(), b.len())
            }));
        }
        if let Some(cb) = h.net_tx {
            let c = Ctx(h.net_ctx);
            hooks.net_tx = Some(Arc::new(move |b: &[u8]| {
                let c = c;
                cb(c.0, b.as_ptr(), b.len())
            }));
        }
    }
    hooks
}

/// Build a VM from a TOML device profile. Returns NULL on error (message in
/// `err`).
#[no_mangle]
pub unsafe extern "C" fn apex_vm_create(
    profile_path: *const c_char,
    hooks: *const ApexHooks,
    err: *mut c_char,
    err_len: usize,
) -> *mut ApexVm {
    guard(std::ptr::null_mut(), || {
        if profile_path.is_null() {
            write_err(err, err_len, "profile path is NULL");
            return std::ptr::null_mut();
        }
        let path = CStr::from_ptr(profile_path).to_string_lossy().into_owned();
        let result = VmConfig::from_file(Path::new(&path)).and_then(|cfg| Machine::build(cfg, hooks_from(hooks)));
        match result {
            Ok(m) => Box::into_raw(Box::new(ApexVm { machine: m, last_error: Mutex::new(None) })),
            Err(e) => {
                write_err(err, err_len, &e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_vm_start(p: *mut ApexVm) -> i32 {
    guard(APEX_ERR_PANIC, || match vm(p) {
        Some(v) => match v.machine.start() {
            Ok(()) => APEX_OK,
            Err(e) => {
                *v.last_error.lock().unwrap() = CString::new(e.to_string()).ok();
                APEX_ERR
            }
        },
        None => APEX_ERR,
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_vm_request_stop(p: *mut ApexVm) {
    guard((), || {
        if let Some(v) = vm(p) {
            v.machine.request_stop();
        }
    })
}

/// Block until the VM stops; returns an `APEX_STOP_*` code.
#[no_mangle]
pub unsafe extern "C" fn apex_vm_wait(p: *mut ApexVm) -> i32 {
    guard(APEX_ERR_PANIC, || match vm(p) {
        Some(v) => {
            let r = v.machine.wait();
            if let StopReason::Error(e) = &r {
                *v.last_error.lock().unwrap() = CString::new(e.clone()).ok();
            }
            stop_code(&r)
        }
        None => APEX_ERR,
    })
}

/// Non-blocking: `APEX_STOP_NONE` while running.
#[no_mangle]
pub unsafe extern "C" fn apex_vm_stop_reason(p: *mut ApexVm) -> i32 {
    guard(APEX_ERR_PANIC, || match vm(p) {
        Some(v) => v.machine.cpus().stop_reason().map(|r| stop_code(&r)).unwrap_or(APEX_STOP_NONE),
        None => APEX_ERR,
    })
}

/// Last error message (valid until the next call on this VM), or NULL.
#[no_mangle]
pub unsafe extern "C" fn apex_vm_last_error(p: *mut ApexVm) -> *const c_char {
    match vm(p) {
        Some(v) => v.last_error.lock().unwrap().as_ref().map(|s| s.as_ptr()).unwrap_or(std::ptr::null()),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn apex_vm_destroy(p: *mut ApexVm) {
    if !p.is_null() {
        guard((), || drop(Box::from_raw(p)));
    }
}

#[no_mangle]
pub unsafe extern "C" fn apex_display_info(p: *mut ApexVm, out: *mut ApexDisplayInfo) {
    guard((), || {
        if let (Some(v), Some(o)) = (vm(p), out.as_mut()) {
            let c = v.machine.controls.display.config();
            *o = ApexDisplayInfo { width: c.width, height: c.height, refresh_hz: c.refresh_hz, dpi: c.dpi };
        }
    })
}

/// Latest frame newer than `after_seq`. On success the slot is pinned until
/// `apex_display_release`.
#[no_mangle]
pub unsafe extern "C" fn apex_display_acquire(p: *mut ApexVm, after_seq: u64, out: *mut FrameInfo) -> bool {
    guard(false, || match (vm(p), out.as_mut()) {
        (Some(v), Some(o)) => match v.machine.controls.display.acquire(after_seq) {
            Some(f) => {
                *o = f;
                true
            }
            None => false,
        },
        _ => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_display_release(p: *mut ApexVm, slot: u32) {
    guard((), || {
        if let Some(v) = vm(p) {
            v.machine.controls.display.release(slot);
        }
    })
}

/// Host display refresh (only when the profile uses `vsync = "host"`).
#[no_mangle]
pub unsafe extern "C" fn apex_display_vsync(p: *mut ApexVm) {
    guard((), || {
        if let Some(v) = vm(p) {
            v.machine.controls.display.vsync();
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_touch_frame(p: *mut ApexVm, contacts: *const ApexTouch, count: u32) {
    guard((), || {
        let Some(v) = vm(p) else { return };
        let list: &[ApexTouch] = if contacts.is_null() || count == 0 { &[] } else { std::slice::from_raw_parts(contacts, count as usize) };
        let c: Vec<Contact> = list.iter().map(|t| Contact { id: t.id, x: t.x, y: t.y, pressure: t.pressure, major: t.major }).collect();
        v.machine.controls.touch.touch_frame(&c);
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_key(p: *mut ApexVm, linux_code: u16, down: bool) {
    guard((), || {
        if let Some(v) = vm(p) {
            let c = &v.machine.controls;
            match (&c.keyboard, apex_devices::virtio::input::is_phone_button(linux_code)) {
                (Some(kb), false) => kb.key(linux_code, down),
                _ => c.buttons.key(linux_code, down),
            }
        }
    })
}

/// Map a macOS virtual key code (kVK_*) to a Linux key code; 0 if unmapped.
#[no_mangle]
pub extern "C" fn apex_mac_keycode_to_linux(mac_vk: u16) -> u16 {
    mac_keycode_to_linux(mac_vk).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn apex_console_input(p: *mut ApexVm, data: *const u8, len: usize) {
    guard((), || {
        if let Some(v) = vm(p) {
            if !data.is_null() && len > 0 {
                v.machine.controls.console.send(std::slice::from_raw_parts(data, len));
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_battery_set(p: *mut ApexVm, percent: u32, charging: bool, ac_online: bool) {
    guard((), || {
        if let Some(v) = vm(p) {
            let b = &v.machine.controls.battery;
            let mut s: BatteryState = b.state();
            s.capacity = percent.min(100);
            s.ac_online = ac_online;
            s.status = if percent >= 100 && ac_online {
                ChargeStatus::Full
            } else if charging {
                ChargeStatus::Charging
            } else if ac_online {
                ChargeStatus::NotCharging
            } else {
                ChargeStatus::Discharging
            };
            s.current_ua = if charging { 1_200_000 } else { -450_000 };
            b.set_state(s);
        }
    })
}

/// Deliver an Ethernet frame from the host network (network.mode = "host").
#[no_mangle]
pub unsafe extern "C" fn apex_net_rx(p: *mut ApexVm, frame: *const u8, len: usize) {
    guard((), || {
        if let Some(v) = vm(p) {
            if let Some(n) = &v.machine.controls.net_rx {
                if !frame.is_null() && len > 0 {
                    n.inject(std::slice::from_raw_parts(frame, len));
                }
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn apex_stats(p: *mut ApexVm, out: *mut ApexStats) {
    use std::sync::atomic::Ordering::Relaxed;
    guard((), || {
        if let (Some(v), Some(o)) = (vm(p), out.as_mut()) {
            let d = v.machine.controls.display.stats();
            let mut s = ApexStats {
                frames_submitted: d.frames_submitted.load(Relaxed),
                frames_presented: d.frames_presented.load(Relaxed),
                vsyncs: d.vsyncs.load(Relaxed),
                ..Default::default()
            };
            for c in &v.machine.cpus().stats {
                s.exits_mmio += c.mmio.load(Relaxed);
                s.exits_sysreg += c.sysreg.load(Relaxed);
                s.exits_wfi += c.wfi.load(Relaxed);
                s.exits_psci += c.psci.load(Relaxed);
                s.exits_vtimer += c.vtimer.load(Relaxed);
            }
            *o = s;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handles_are_harmless() {
        unsafe {
            assert_eq!(apex_vm_start(std::ptr::null_mut()), APEX_ERR);
            apex_vm_request_stop(std::ptr::null_mut());
            apex_touch_frame(std::ptr::null_mut(), std::ptr::null(), 0);
            apex_vm_destroy(std::ptr::null_mut());
            let mut f = std::mem::zeroed::<FrameInfo>();
            assert!(!apex_display_acquire(std::ptr::null_mut(), 0, &mut f));
        }
        assert_eq!(apex_mac_keycode_to_linux(0x31), 57);
        let v = unsafe { CStr::from_ptr(apex_version()) };
        assert_eq!(v.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn create_reports_errors() {
        let mut buf = [0 as c_char; 256];
        let p = CString::new("/nonexistent/profile.toml").unwrap();
        let vm = unsafe { apex_vm_create(p.as_ptr(), std::ptr::null(), buf.as_mut_ptr(), buf.len()) };
        assert!(vm.is_null());
        let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        assert!(msg.contains("profile.toml"), "{msg}");
    }
}
