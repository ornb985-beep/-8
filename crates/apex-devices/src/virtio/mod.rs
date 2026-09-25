//! virtio 1.2 core: device trait, interrupt handling, split virtqueues and
//! the virtio-mmio transport.

pub mod blk;
pub mod console;
pub mod disk;
pub mod gpu;
pub mod input;
pub mod mmio;
pub mod net;
pub mod queue;
pub mod rng;

use std::sync::{Arc, Mutex};

use apex_core::irq::IrqLine;
use apex_core::mem::GuestMemory;
use apex_core::Result;

pub use queue::{DescriptorChain, Queue, Reader, Writer};

/// Device IDs (virtio spec 5).
pub mod device_type {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const CONSOLE: u32 = 3;
    pub const RNG: u32 = 4;
    pub const BALLOON: u32 = 5;
    pub const GPU: u32 = 16;
    pub const INPUT: u32 = 18;
    pub const VSOCK: u32 = 19;
    pub const SOUND: u32 = 25;
}

/// Transport-level feature bits.
pub mod features {
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
    pub const RING_PACKED: u64 = 1 << 34;
    pub const IN_ORDER: u64 = 1 << 35;
}

/// Device status bits.
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

pub const INT_USED_RING: u32 = 1;
pub const INT_CONFIG: u32 = 2;

/// Interrupt status shared between the transport and the device. The line
/// is level triggered and stays asserted until the driver acknowledges every
/// pending cause.
#[derive(Clone)]
pub struct VirtioInterrupt {
    inner: Arc<IrqInner>,
}

struct IrqInner {
    status: Mutex<u32>,
    line: IrqLine,
}

impl VirtioInterrupt {
    pub fn new(line: IrqLine) -> Self {
        VirtioInterrupt { inner: Arc::new(IrqInner { status: Mutex::new(0), line }) }
    }

    fn raise(&self, bits: u32) {
        let mut s = self.inner.status.lock().unwrap();
        *s |= bits;
        self.inner.line.set_level(true);
    }

    #[inline]
    pub fn signal_used_queue(&self) {
        self.raise(INT_USED_RING);
    }

    pub fn signal_config_changed(&self) {
        self.raise(INT_CONFIG);
    }

    pub fn status(&self) -> u32 {
        *self.inner.status.lock().unwrap()
    }

    pub fn ack(&self, bits: u32) {
        let mut s = self.inner.status.lock().unwrap();
        *s &= !bits;
        if *s == 0 {
            self.inner.line.set_level(false);
        }
    }

    pub fn reset(&self) {
        let mut s = self.inner.status.lock().unwrap();
        *s = 0;
        self.inner.line.set_level(false);
    }
}

/// A virtio shared memory region (virtio 1.2, 2.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShmRegion {
    pub id: u8,
    pub base: u64,
    pub len: u64,
}

/// What a device receives when the driver sets DRIVER_OK.
pub struct ActivateContext {
    pub mem: GuestMemory,
    pub queues: Vec<Queue>,
    pub interrupt: VirtioInterrupt,
    /// Negotiated feature set.
    pub features: u64,
}

pub trait VirtioDevice: Send {
    fn device_type(&self) -> u32;
    fn name(&self) -> &str;
    fn queue_max_sizes(&self) -> Vec<u16>;
    /// Device specific feature bits. The transport adds VERSION_1,
    /// EVENT_IDX and INDIRECT_DESC.
    fn device_features(&self) -> u64;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, ctx: ActivateContext) -> Result<()>;
    /// The driver kicked queue `index` (called on the vCPU thread).
    fn queue_notify(&mut self, index: u16);
    /// Device reset: stop workers and forget all queue state.
    fn reset(&mut self);
    fn shm_regions(&self) -> Vec<ShmRegion> {
        Vec::new()
    }
}

/// Helper for config space reads out of a byte image.
pub fn read_config_bytes(cfg: &[u8], offset: u64, data: &mut [u8]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
    }
}
