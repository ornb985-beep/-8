//! Minimal hand-written libc surface.
//!
//! Only the handful of calls the VMM needs are declared here so the whole
//! project builds without the `libc` crate. Constants differ between Darwin
//! and Linux and are selected with `cfg`.

#![allow(non_camel_case_types)]

use std::ffi::c_void;
use std::io;

pub type c_int = i32;
pub type size_t = usize;
pub type off_t = i64;

pub const PROT_NONE: c_int = 0;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;

pub const MAP_SHARED: c_int = 0x0001;
pub const MAP_PRIVATE: c_int = 0x0002;

#[cfg(target_os = "macos")]
pub const MAP_ANON: c_int = 0x1000;
#[cfg(not(target_os = "macos"))]
pub const MAP_ANON: c_int = 0x20;

#[cfg(target_os = "macos")]
pub const MAP_NORESERVE: c_int = 0x40;
#[cfg(not(target_os = "macos"))]
pub const MAP_NORESERVE: c_int = 0x4000;

pub const MADV_DONTNEED: c_int = 4;
#[cfg(target_os = "macos")]
pub const MADV_FREE: c_int = 5;
#[cfg(not(target_os = "macos"))]
pub const MADV_FREE: c_int = 8;

#[cfg(target_os = "macos")]
const SC_PAGESIZE: c_int = 29;
#[cfg(not(target_os = "macos"))]
const SC_PAGESIZE: c_int = 30;

#[cfg(target_os = "macos")]
const SC_NPROCESSORS_ONLN: c_int = 58;
#[cfg(not(target_os = "macos"))]
const SC_NPROCESSORS_ONLN: c_int = 84;

const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

extern "C" {
    fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: size_t) -> c_int;
    fn madvise(addr: *mut c_void, len: size_t, advice: c_int) -> c_int;
    fn mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int;
    fn sysconf(name: c_int) -> i64;
    fn getentropy(buf: *mut c_void, len: size_t) -> c_int;
}

#[cfg(target_os = "macos")]
extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

#[cfg(target_os = "linux")]
extern "C" {
    fn fallocate(fd: c_int, mode: c_int, offset: off_t, len: off_t) -> c_int;
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct FPunchHole {
    fp_flags: u32,
    reserved: u32,
    fp_offset: off_t,
    fp_length: off_t,
}

/// Deallocate a byte range of a file, keeping its size (virtio-blk DISCARD).
/// Offsets should be aligned to the file system block size.
pub fn punch_hole(fd: c_int, offset: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        const F_PUNCHHOLE: c_int = 99;
        let arg = FPunchHole { fp_flags: 0, reserved: 0, fp_offset: offset as off_t, fp_length: len as off_t };
        // SAFETY: valid fd and pointer to a properly laid out struct.
        if unsafe { fcntl(fd, F_PUNCHHOLE, &arg as *const FPunchHole) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        const FALLOC_FL_KEEP_SIZE: c_int = 1;
        const FALLOC_FL_PUNCH_HOLE: c_int = 2;
        // SAFETY: plain syscall on a caller-provided fd.
        if unsafe { fallocate(fd, FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE, offset as off_t, len as off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (fd, offset, len);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// Anonymous, private, zero-filled mapping. Pages are committed lazily by the
/// host kernel, which is what lets an 8 GiB guest start with a few MiB of RSS.
pub fn mmap_anonymous(len: usize) -> io::Result<*mut u8> {
    // SAFETY: plain anonymous mapping with no fixed address.
    let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0) };
    if p == MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(p as *mut u8)
    }
}

/// Reserve address space without backing it (PROT_NONE).
pub fn mmap_reserve(len: usize) -> io::Result<*mut u8> {
    // SAFETY: anonymous PROT_NONE reservation.
    let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_NONE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0) };
    if p == MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(p as *mut u8)
    }
}

/// # Safety
/// `ptr`/`len` must describe a mapping previously returned by `mmap_*`.
pub unsafe fn munmap_raw(ptr: *mut u8, len: usize) -> io::Result<()> {
    if munmap(ptr as *mut c_void, len) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// # Safety
/// Range must be inside a live mapping owned by the caller.
pub unsafe fn mprotect_raw(ptr: *mut u8, len: usize, prot: c_int) -> io::Result<()> {
    if mprotect(ptr as *mut c_void, len, prot) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Give pages back to the host (free page reporting / balloon).
///
/// # Safety
/// Range must be inside a live anonymous mapping owned by the caller.
pub unsafe fn discard_pages(ptr: *mut u8, len: usize) -> io::Result<()> {
    if madvise(ptr as *mut c_void, len, MADV_DONTNEED) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Host page size (16 KiB on Apple Silicon, 4 KiB on most Linux hosts).
pub fn host_page_size() -> usize {
    // SAFETY: sysconf is always safe to call.
    let v = unsafe { sysconf(SC_PAGESIZE) };
    if v <= 0 {
        4096
    } else {
        v as usize
    }
}

pub fn online_cpus() -> usize {
    // SAFETY: sysconf is always safe to call.
    let v = unsafe { sysconf(SC_NPROCESSORS_ONLN) };
    if v <= 0 {
        1
    } else {
        v as usize
    }
}

/// Fill `buf` with cryptographically secure random bytes.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    for chunk in buf.chunks_mut(256) {
        // SAFETY: getentropy writes at most 256 bytes into the provided slice.
        if unsafe { getentropy(chunk.as_mut_ptr() as *mut c_void, chunk.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, relative_priority: c_int) -> c_int;
}

/// Ask the scheduler to treat the calling thread as latency critical. On
/// Apple Silicon this steers vCPU, vsync and GPU threads onto P-cores.
pub fn set_thread_latency_critical() {
    #[cfg(target_os = "macos")]
    {
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        // SAFETY: affects only the calling thread.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    }
}

/// Raw host tick counter. On Apple Silicon this is the same 24 MHz counter
/// the guest reads through CNTVCT_EL0 (minus the VM's vtimer offset).
#[cfg(target_os = "macos")]
pub fn host_ticks() -> u64 {
    // SAFETY: no preconditions.
    unsafe { mach_absolute_time() }
}

#[cfg(not(target_os = "macos"))]
pub fn host_ticks() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let s = START.get_or_init(Instant::now);
    // Emulate a 24 MHz counter so the rest of the code sees identical units.
    (s.elapsed().as_nanos() as u64).saturating_mul(3) / 125
}

/// Nanoseconds per host tick as a (numer, denom) pair.
pub fn host_timebase() -> (u32, u32) {
    #[cfg(target_os = "macos")]
    {
        let mut info = MachTimebaseInfo::default();
        // SAFETY: valid out-pointer.
        unsafe { mach_timebase_info(&mut info) };
        if info.denom == 0 {
            (1, 1)
        } else {
            (info.numer, info.denom)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        (125, 3)
    }
}

/// Convert host ticks to nanoseconds.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let (n, d) = host_timebase();
    ((ticks as u128 * n as u128) / d as u128) as u64
}

/// Convert nanoseconds to host ticks.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let (n, d) = host_timebase();
    ((ns as u128 * d as u128) / n.max(1) as u128) as u64
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct TimeConstraintPolicy {
    period: u32,
    computation: u32,
    constraint: u32,
    preemptible: i32,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_wait_until(deadline: u64) -> c_int;
    fn pthread_self() -> usize;
    fn pthread_mach_thread_np(thread: usize) -> u32;
    fn thread_policy_set(thread: u32, flavor: u32, policy: *mut i32, count: u32) -> c_int;
}

/// Block until the host counter reaches `deadline` (absolute, in ticks).
/// On macOS this is `mach_wait_until`, which together with a time
/// constraint policy wakes within tens of microseconds.
pub fn sleep_until_ticks(deadline: u64) {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain Mach trap on the calling thread.
        unsafe { mach_wait_until(deadline) };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let now = host_ticks();
        if deadline > now {
            std::thread::sleep(std::time::Duration::from_nanos(ticks_to_ns(deadline - now)));
        }
    }
}

/// Give the calling thread a Mach real-time "time constraint" policy (the
/// scheduling class CoreAudio and CoreVideo use): the kernel guarantees
/// `computation_ns` of CPU within `constraint_ns` of every `period_ns`.
/// Used for the virtual vsync so frame pacing does not depend on timer
/// coalescing or system load. Returns false where unsupported.
pub fn set_thread_time_constraint(period_ns: u64, computation_ns: u64, constraint_ns: u64) -> bool {
    #[cfg(target_os = "macos")]
    {
        const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
        const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;
        let mut p = TimeConstraintPolicy {
            period: ns_to_ticks(period_ns) as u32,
            computation: ns_to_ticks(computation_ns) as u32,
            constraint: ns_to_ticks(constraint_ns) as u32,
            preemptible: 1,
        };
        // SAFETY: valid policy struct for the calling thread's Mach port.
        let r = unsafe {
            thread_policy_set(
                pthread_mach_thread_np(pthread_self()),
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut p as *mut TimeConstraintPolicy as *mut i32,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };
        r == 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (period_ns, computation_ns, constraint_ns);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anon_mapping_is_zeroed_and_writable() {
        let len = host_page_size() * 4;
        let p = mmap_anonymous(len).unwrap();
        unsafe {
            assert_eq!(*p, 0);
            *p.add(len - 1) = 7;
            assert_eq!(*p.add(len - 1), 7);
            discard_pages(p, len).unwrap();
            // Linux guarantees zero-fill after MADV_DONTNEED on private anon memory.
            #[cfg(target_os = "linux")]
            assert_eq!(*p.add(len - 1), 0);
            munmap_raw(p, len).unwrap();
        }
    }

    #[test]
    fn entropy_and_ticks() {
        let mut a = [0u8; 600];
        fill_random(&mut a).unwrap();
        assert!(a.iter().any(|&b| b != 0));
        let t0 = host_ticks();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let dt = ticks_to_ns(host_ticks() - t0);
        assert!(dt >= 1_000_000, "dt={dt}");
    }
}
