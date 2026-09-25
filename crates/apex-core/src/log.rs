//! Minimal leveled logger. Output goes to stderr by default; the macOS
//! frontend installs a sink through the C ABI so logs show up in its console.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{OnceLock, RwLock};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.to_ascii_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
            Level::Trace => "T",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub type Sink = Box<dyn Fn(Level, &str) + Send + Sync>;
fn sink() -> &'static RwLock<Option<Sink>> {
    static SINK: OnceLock<RwLock<Option<Sink>>> = OnceLock::new();
    SINK.get_or_init(|| RwLock::new(None))
}

fn start() -> std::time::Instant {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

pub fn max_level() -> Level {
    let v = LEVEL.load(Ordering::Relaxed);
    if v == 0 {
        let lvl = std::env::var("APEX_LOG").ok().and_then(|s| Level::parse(&s)).unwrap_or(Level::Info);
        LEVEL.store(lvl as u8, Ordering::Relaxed);
        start();
        return lvl;
    }
    // SAFETY: only valid discriminants are ever stored.
    unsafe { std::mem::transmute::<u8, Level>(v) }
}

pub fn set_max_level(l: Level) {
    LEVEL.store(l as u8, Ordering::Relaxed);
}

pub fn set_sink(s: Option<Sink>) {
    *sink().write().unwrap() = s;
}

#[inline]
pub fn enabled(l: Level) -> bool {
    l <= max_level()
}

pub fn log(l: Level, target: &str, msg: std::fmt::Arguments<'_>) {
    let text = format!("{}", msg);
    if let Some(s) = sink().read().unwrap().as_ref() {
        s(l, &format!("[{target}] {text}"));
        return;
    }
    let t = start().elapsed();
    eprintln!("{:>6}.{:06} {} {:<10} {}", t.as_secs(), t.subsec_micros(), l.tag(), target, text);
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $($arg:tt)*) => {
        if $crate::log::enabled($lvl) {
            $crate::log::log($lvl, module_path!().rsplit("::").next().unwrap_or(""), format_args!($($arg)*));
        }
    };
}
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $($arg)*) }; }
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $($arg)*) }; }
#[macro_export]
macro_rules! info { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $($arg)*) }; }
#[macro_export]
macro_rules! debug { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $($arg)*) }; }
#[macro_export]
macro_rules! trace { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $($arg)*) }; }
