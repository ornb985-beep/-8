//! Small synchronization primitives (the VMM does not use an async runtime:
//! every device worker is a plain thread parked on an [`Event`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A counting doorbell similar to Linux `eventfd`: `signal` adds to the
/// counter, `wait` blocks until it is non-zero and resets it.
#[derive(Default)]
pub struct Event {
    count: Mutex<u64>,
    cv: Condvar,
}

impl Event {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn signal(&self) {
        let mut c = self.count.lock().unwrap();
        *c = c.saturating_add(1);
        self.cv.notify_all();
    }

    /// Block until signalled; returns the accumulated count.
    pub fn wait(&self) -> u64 {
        let mut c = self.count.lock().unwrap();
        while *c == 0 {
            c = self.cv.wait(c).unwrap();
        }
        std::mem::take(&mut *c)
    }

    /// Wait with a timeout; returns 0 on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        self.wait_until(deadline)
    }

    pub fn wait_until(&self, deadline: Instant) -> u64 {
        let mut c = self.count.lock().unwrap();
        while *c == 0 {
            let now = Instant::now();
            if now >= deadline {
                return 0;
            }
            c = self.cv.wait_timeout(c, deadline - now).unwrap().0;
        }
        std::mem::take(&mut *c)
    }

    pub fn try_take(&self) -> u64 {
        std::mem::take(&mut *self.count.lock().unwrap())
    }
}

/// Cooperative stop flag shared by worker threads.
#[derive(Default)]
pub struct StopFlag(AtomicBool);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Precise periodic ticker used for the virtual vsync.
///
/// Deadlines are absolute host counter values (the same 24 MHz clock the
/// guest's CNTVCT is derived from), so they never drift. The thread sleeps
/// with `sleep_until_ticks` (`mach_wait_until` on macOS) and spins only for
/// the last `spin` interval.
pub struct Ticker {
    period: Duration,
    period_ticks: u64,
    next: u64,
    spin_ticks: u64,
}

impl Ticker {
    pub fn new(period: Duration) -> Self {
        let period_ticks = crate::sys::ns_to_ticks(period.as_nanos() as u64).max(1);
        Ticker { period, period_ticks, next: crate::sys::host_ticks() + period_ticks, spin_ticks: crate::sys::ns_to_ticks(100_000) }
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// Request real-time scheduling for the calling thread, sized for this
    /// ticker's period. Returns false if the OS refused or lacks support.
    pub fn make_thread_realtime(&self) -> bool {
        let p = self.period.as_nanos() as u64;
        crate::sys::set_thread_time_constraint(p, (p / 8).clamp(100_000, 2_000_000), (p / 4).clamp(200_000, 4_000_000))
    }

    /// Sleep until the next tick. Returns the number of periods that elapsed
    /// (more than 1 means deadlines were missed and the ticker skipped ahead
    /// instead of bursting).
    pub fn wait(&mut self) -> u64 {
        let target = self.next;
        if target > crate::sys::host_ticks() + self.spin_ticks {
            crate::sys::sleep_until_ticks(target - self.spin_ticks);
        }
        while crate::sys::host_ticks() < target {
            std::hint::spin_loop();
        }
        let now = crate::sys::host_ticks();
        let mut ticks = 1;
        self.next += self.period_ticks;
        while self.next <= now {
            self.next += self.period_ticks;
            ticks += 1;
        }
        ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn event_counts_and_times_out() {
        let e = Arc::new(Event::new());
        assert_eq!(e.wait_timeout(Duration::from_millis(5)), 0);
        e.signal();
        e.signal();
        assert_eq!(e.wait(), 2);
        let e2 = e.clone();
        let t = std::thread::spawn(move || e2.wait());
        std::thread::sleep(Duration::from_millis(5));
        e.signal();
        assert_eq!(t.join().unwrap(), 1);
    }

    #[test]
    fn ticker_is_periodic() {
        let mut t = Ticker::new(Duration::from_millis(2));
        let start = Instant::now();
        let mut n = 0;
        for _ in 0..10 {
            n += t.wait();
        }
        let el = start.elapsed();
        assert!(n >= 10);
        assert!(el >= Duration::from_millis(19), "{el:?}");
    }
}
