//! Apex-AOSP core primitives.
//!
//! This crate is platform independent and has no dependencies. Everything that
//! touches the host kernel goes through the tiny FFI surface in [`sys`], which
//! is implemented for both macOS (the production host) and Linux (CI / unit
//! tests).

/// Tiny local replacement for the `bitflags` crate.
#[macro_export]
macro_rules! bitflags_lite {
    (pub struct $name:ident: $t:ty { $(const $f:ident = $v:expr;)* }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(pub $t);
        impl $name {
            $(pub const $f: $name = $name($v);)*
            pub const fn bits(self) -> $t { self.0 }
            pub const fn empty() -> Self { $name(0) }
            pub const fn contains(self, o: Self) -> bool { self.0 & o.0 == o.0 }
        }
        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, o: Self) -> Self { $name(self.0 | o.0) }
        }
        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, o: Self) { self.0 |= o.0 }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({:#x})", stringify!($name), self.0)
            }
        }
    };
}

pub mod bus;
pub mod crc;
pub mod error;
pub mod fdt;
pub mod hv;
pub mod irq;
pub mod log;
pub mod mem;
pub mod sync;
pub mod sys;
pub mod toml;

pub use error::{Error, Result};

/// Round `v` up to the next multiple of `align` (which must be a power of two).
#[inline]
pub const fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// Round `v` down to a multiple of `align` (which must be a power of two).
#[inline]
pub const fn align_down(v: u64, align: u64) -> u64 {
    v & !(align - 1)
}

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// Parse a human size such as `8G`, `512M`, `4096K`, `1048576` into bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Config("empty size".into()));
    }
    let (num, mul) = match t.as_bytes()[t.len() - 1].to_ascii_uppercase() {
        b'K' => (&t[..t.len() - 1], KIB),
        b'M' => (&t[..t.len() - 1], MIB),
        b'G' => (&t[..t.len() - 1], GIB),
        b'T' => (&t[..t.len() - 1], 1024 * GIB),
        b'B' => return parse_size(&t[..t.len() - 1]),
        _ => (t, 1),
    };
    let n: u64 = num.trim().replace('_', "").parse().map_err(|_| Error::Config(format!("invalid size `{s}`")))?;
    n.checked_mul(mul).ok_or_else(|| Error::Config(format!("size overflow `{s}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8G").unwrap(), 8 * GIB);
        assert_eq!(parse_size("512m").unwrap(), 512 * MIB);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("1_024K").unwrap(), 1024 * KIB);
        assert_eq!(parse_size("2GB").unwrap(), 2 * GIB);
        assert!(parse_size("x").is_err());
        assert_eq!(align_up(5, 4), 8);
        assert_eq!(align_down(0x1fff, 0x1000), 0x1000);
    }
}
