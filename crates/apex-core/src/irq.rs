//! Interrupt routing from devices to the interrupt controller.
//!
//! Devices only ever see an [`IrqLine`]. Whether the line ends up in Apple's
//! in-kernel vGICv3 (`hv_gic_set_spi`, macOS 15+) or in the userspace GICv3
//! model is decided once when the VM is created.

use std::fmt;
use std::sync::Arc;

/// First GIC INTID used for shared peripheral interrupts.
pub const SPI_BASE: u32 = 32;

pub trait InterruptController: Send + Sync {
    /// Drive the level of SPI `spi` (0-based, INTID = 32 + spi).
    fn set_spi_level(&self, spi: u32, level: bool);
    /// Number of SPIs the controller implements.
    fn spi_count(&self) -> u32;
}

#[derive(Clone)]
pub struct IrqLine {
    ctrl: Arc<dyn InterruptController>,
    spi: u32,
}

impl fmt::Debug for IrqLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IrqLine(spi {})", self.spi)
    }
}

impl IrqLine {
    pub fn new(ctrl: Arc<dyn InterruptController>, spi: u32) -> Self {
        IrqLine { ctrl, spi }
    }

    /// SPI number as used in the device tree `interrupts = <0 spi 4>` cell.
    pub fn spi(&self) -> u32 {
        self.spi
    }

    #[inline]
    pub fn set_level(&self, level: bool) {
        self.ctrl.set_spi_level(self.spi, level);
    }

    /// Edge semantics on top of a level-sensitive line.
    #[inline]
    pub fn pulse(&self) {
        self.ctrl.set_spi_level(self.spi, true);
        self.ctrl.set_spi_level(self.spi, false);
    }
}

/// An interrupt controller that records line levels; used by unit tests of
/// device models.
#[derive(Default)]
pub struct RecordingIrqChip {
    levels: std::sync::Mutex<std::collections::HashMap<u32, bool>>,
    edges: std::sync::atomic::AtomicU64,
}

impl RecordingIrqChip {
    pub fn level(&self, spi: u32) -> bool {
        *self.levels.lock().unwrap().get(&spi).unwrap_or(&false)
    }
    pub fn rising_edges(&self) -> u64 {
        self.edges.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl InterruptController for RecordingIrqChip {
    fn set_spi_level(&self, spi: u32, level: bool) {
        let mut l = self.levels.lock().unwrap();
        let prev = l.insert(spi, level).unwrap_or(false);
        if level && !prev {
            self.edges.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn spi_count(&self) -> u32 {
        988
    }
}
