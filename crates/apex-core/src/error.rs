use std::fmt;

/// Unified error type for the VMM.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Config(String),
    Memory(String),
    Hypervisor(String),
    Boot(String),
    Device(String),
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Config(s) => write!(f, "configuration error: {s}"),
            Error::Memory(s) => write!(f, "guest memory error: {s}"),
            Error::Hypervisor(s) => write!(f, "hypervisor error: {s}"),
            Error::Boot(s) => write!(f, "boot error: {s}"),
            Error::Device(s) => write!(f, "device error: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
