//! virtio-mmio transport, version 2 (virtio 1.2, 4.2).

use std::sync::Mutex;

use apex_core::bus::MmioDevice;
use apex_core::mem::{le_read, le_write, GuestAddress, GuestMemory};

use super::{features, status, ActivateContext, Queue, VirtioDevice, VirtioInterrupt};

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
/// "APEX" — shows up in /sys/bus/virtio/devices/*/vendor.
pub const VENDOR_ID: u32 = 0x5845_5041;

mod reg {
    pub const MAGIC: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_DRIVER_LOW: u64 = 0x090;
    pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
    pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
    pub const SHM_SEL: u64 = 0x0ac;
    pub const SHM_LEN_LOW: u64 = 0x0b0;
    pub const SHM_LEN_HIGH: u64 = 0x0b4;
    pub const SHM_BASE_LOW: u64 = 0x0b8;
    pub const SHM_BASE_HIGH: u64 = 0x0bc;
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    pub const CONFIG: u64 = 0x100;
}

struct State {
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    queue_sel: u32,
    queues: Vec<Queue>,
    shm_sel: u32,
    config_generation: u32,
    activated: bool,
}

pub struct MmioTransport {
    state: Mutex<State>,
    device: Mutex<Box<dyn VirtioDevice>>,
    interrupt: VirtioInterrupt,
    mem: GuestMemory,
    device_id: u32,
    device_features: u64,
    name: String,
}

impl MmioTransport {
    pub fn new(device: Box<dyn VirtioDevice>, mem: GuestMemory, interrupt: VirtioInterrupt) -> MmioTransport {
        let queues = device.queue_max_sizes().into_iter().map(Queue::new).collect();
        let device_features = device.device_features() | features::VERSION_1 | features::RING_EVENT_IDX | features::RING_INDIRECT_DESC;
        MmioTransport {
            state: Mutex::new(State {
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                status: 0,
                queue_sel: 0,
                queues,
                shm_sel: 0,
                config_generation: 0,
                activated: false,
            }),
            device_id: device.device_type(),
            name: format!("virtio-{}", device.name()),
            device: Mutex::new(device),
            interrupt,
            mem,
            device_features,
        }
    }

    pub fn interrupt(&self) -> &VirtioInterrupt {
        &self.interrupt
    }

    /// Bump the config generation and raise a config-change interrupt (used
    /// e.g. when the display resolution changes).
    pub fn config_changed(&self) {
        self.state.lock().unwrap().config_generation += 1;
        self.interrupt.signal_config_changed();
    }

    fn reset(&self, st: &mut State) {
        self.device.lock().unwrap().reset();
        for q in &mut st.queues {
            q.reset();
        }
        st.driver_features = 0;
        st.driver_features_sel = 0;
        st.device_features_sel = 0;
        st.queue_sel = 0;
        st.status = 0;
        st.activated = false;
        self.interrupt.reset();
    }

    fn set_status(&self, st: &mut State, v: u32) {
        if v == 0 {
            self.reset(st);
            return;
        }
        let newly = v & !st.status;
        if newly & status::FEATURES_OK != 0 {
            let valid = st.driver_features & !self.device_features == 0 && st.driver_features & features::VERSION_1 != 0;
            if !valid {
                apex_core::warn!("{}: driver accepted unsupported features {:#x}", self.name, st.driver_features);
                st.status = v & !status::FEATURES_OK;
                return;
            }
        }
        st.status = v;
        if newly & status::DRIVER_OK != 0 && st.status & status::FEATURES_OK != 0 && !st.activated {
            let event_idx = st.driver_features & features::RING_EVENT_IDX != 0;
            let mut queues = st.queues.clone();
            for q in &mut queues {
                q.set_event_idx(event_idx);
                if q.ready {
                    if let Err(e) = q.validate(&self.mem) {
                        apex_core::error!("{}: {e}", self.name);
                        st.status |= status::DEVICE_NEEDS_RESET;
                        self.interrupt.signal_config_changed();
                        return;
                    }
                }
            }
            let ctx = ActivateContext { mem: self.mem.clone(), queues, interrupt: self.interrupt.clone(), features: st.driver_features };
            match self.device.lock().unwrap().activate(ctx) {
                Ok(()) => {
                    st.activated = true;
                    apex_core::debug!("{} activated (features {:#x})", self.name, st.driver_features);
                }
                Err(e) => {
                    apex_core::error!("{}: activation failed: {e}", self.name);
                    st.status |= status::DEVICE_NEEDS_RESET;
                    self.interrupt.signal_config_changed();
                }
            }
        }
    }

    fn with_queue<R>(st: &mut State, f: impl FnOnce(&mut Queue) -> R) -> Option<R> {
        let i = st.queue_sel as usize;
        st.queues.get_mut(i).map(f)
    }
}

fn set_lo(v: &mut GuestAddress, lo: u32) {
    v.0 = (v.0 & !0xffff_ffff) | lo as u64;
}
fn set_hi(v: &mut GuestAddress, hi: u32) {
    v.0 = (v.0 & 0xffff_ffff) | ((hi as u64) << 32);
}

impl MmioDevice for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if offset >= reg::CONFIG {
            self.device.lock().unwrap().read_config(offset - reg::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            le_write(data, 0);
            return;
        }
        let st = self.state.lock().unwrap();
        let v: u32 = match offset {
            reg::MAGIC => MAGIC,
            reg::VERSION => VERSION,
            reg::DEVICE_ID => self.device_id,
            reg::VENDOR_ID => VENDOR_ID,
            reg::DEVICE_FEATURES => match st.device_features_sel {
                0 => self.device_features as u32,
                1 => (self.device_features >> 32) as u32,
                _ => 0,
            },
            reg::QUEUE_NUM_MAX => st.queues.get(st.queue_sel as usize).map(|q| q.max_size as u32).unwrap_or(0),
            reg::QUEUE_READY => st.queues.get(st.queue_sel as usize).map(|q| q.ready as u32).unwrap_or(0),
            reg::INTERRUPT_STATUS => self.interrupt.status(),
            reg::STATUS => st.status,
            reg::SHM_LEN_LOW | reg::SHM_LEN_HIGH | reg::SHM_BASE_LOW | reg::SHM_BASE_HIGH => {
                let regions = self.device.lock().unwrap().shm_regions();
                match regions.iter().find(|r| r.id as u32 == st.shm_sel) {
                    Some(r) => match offset {
                        reg::SHM_LEN_LOW => r.len as u32,
                        reg::SHM_LEN_HIGH => (r.len >> 32) as u32,
                        reg::SHM_BASE_LOW => r.base as u32,
                        _ => (r.base >> 32) as u32,
                    },
                    // Non-existent region: length reads as all ones.
                    None => 0xffff_ffff,
                }
            }
            reg::CONFIG_GENERATION => st.config_generation,
            _ => 0,
        };
        le_write(data, v as u64);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset >= reg::CONFIG {
            self.device.lock().unwrap().write_config(offset - reg::CONFIG, data);
            return;
        }
        if data.len() != 4 {
            return;
        }
        let v = le_read(data) as u32;
        if offset == reg::QUEUE_NOTIFY {
            // Hot path: do not take the transport lock.
            self.device.lock().unwrap().queue_notify(v as u16);
            return;
        }
        let mut st = self.state.lock().unwrap();
        let configurable = st.status & status::DRIVER_OK == 0;
        match offset {
            reg::DEVICE_FEATURES_SEL => st.device_features_sel = v,
            reg::DRIVER_FEATURES_SEL => st.driver_features_sel = v,
            reg::DRIVER_FEATURES => {
                if st.status & status::FEATURES_OK == 0 {
                    match st.driver_features_sel {
                        0 => st.driver_features = (st.driver_features & !0xffff_ffff) | v as u64,
                        1 => st.driver_features = (st.driver_features & 0xffff_ffff) | ((v as u64) << 32),
                        _ => {}
                    }
                }
            }
            reg::QUEUE_SEL => st.queue_sel = v,
            reg::QUEUE_NUM if configurable => {
                Self::with_queue(&mut st, |q| q.size = v as u16);
            }
            reg::QUEUE_READY if configurable => {
                Self::with_queue(&mut st, |q| q.ready = v & 1 != 0);
            }
            reg::QUEUE_DESC_LOW if configurable => {
                Self::with_queue(&mut st, |q| set_lo(&mut q.desc_table, v));
            }
            reg::QUEUE_DESC_HIGH if configurable => {
                Self::with_queue(&mut st, |q| set_hi(&mut q.desc_table, v));
            }
            reg::QUEUE_DRIVER_LOW if configurable => {
                Self::with_queue(&mut st, |q| set_lo(&mut q.avail_ring, v));
            }
            reg::QUEUE_DRIVER_HIGH if configurable => {
                Self::with_queue(&mut st, |q| set_hi(&mut q.avail_ring, v));
            }
            reg::QUEUE_DEVICE_LOW if configurable => {
                Self::with_queue(&mut st, |q| set_lo(&mut q.used_ring, v));
            }
            reg::QUEUE_DEVICE_HIGH if configurable => {
                Self::with_queue(&mut st, |q| set_hi(&mut q.used_ring, v));
            }
            reg::INTERRUPT_ACK => self.interrupt.ack(v),
            reg::STATUS => self.set_status(&mut st, v),
            reg::SHM_SEL => st.shm_sel = v,
            _ => {}
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::test_driver::Driver;
    use crate::virtio::{ActivateContext, VirtioDevice};
    use apex_core::irq::{IrqLine, RecordingIrqChip};
    use apex_core::Result;
    use std::sync::Arc;

    /// Echo device: copies each readable buffer into the writable part.
    struct Echo {
        ctx: Option<ActivateContext>,
    }
    impl VirtioDevice for Echo {
        fn device_type(&self) -> u32 {
            42
        }
        fn name(&self) -> &str {
            "echo"
        }
        fn queue_max_sizes(&self) -> Vec<u16> {
            vec![8]
        }
        fn device_features(&self) -> u64 {
            1 << 3
        }
        fn read_config(&self, off: u64, data: &mut [u8]) {
            crate::virtio::read_config_bytes(b"CONFIGDATA", off, data)
        }
        fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
            self.ctx = Some(ctx);
            Ok(())
        }
        fn queue_notify(&mut self, _i: u16) {
            let ctx = self.ctx.as_mut().unwrap();
            let q = &mut ctx.queues[0];
            while let Some(c) = q.pop(&ctx.mem) {
                let data = c.reader(&ctx.mem).read_to_vec(4096);
                let n = c.writer(&ctx.mem).write(&data);
                q.add_used(&ctx.mem, c.head, n as u32).unwrap();
            }
            if q.needs_notification(&ctx.mem) {
                ctx.interrupt.signal_used_queue();
            }
        }
        fn reset(&mut self) {
            self.ctx = None;
        }
    }

    fn w(t: &MmioTransport, off: u64, v: u32) {
        t.write(off, &v.to_le_bytes());
    }
    fn r(t: &MmioTransport, off: u64) -> u32 {
        let mut b = [0u8; 4];
        t.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn full_driver_handshake() {
        let mut drv = Driver::new(8);
        let chip = Arc::new(RecordingIrqChip::default());
        let irq = VirtioInterrupt::new(IrqLine::new(chip.clone(), 5));
        let t = MmioTransport::new(Box::new(Echo { ctx: None }), drv.mem.clone(), irq);
        assert_eq!(r(&t, 0), MAGIC);
        assert_eq!(r(&t, 4), 2);
        assert_eq!(r(&t, 8), 42);
        w(&t, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        w(&t, reg::DEVICE_FEATURES_SEL, 1);
        assert_eq!(r(&t, reg::DEVICE_FEATURES) & 1, 1); // VERSION_1
        w(&t, reg::DRIVER_FEATURES_SEL, 0);
        w(&t, reg::DRIVER_FEATURES, (1 << 3) | (1 << 29));
        w(&t, reg::DRIVER_FEATURES_SEL, 1);
        w(&t, reg::DRIVER_FEATURES, 1);
        w(&t, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
        assert_ne!(r(&t, reg::STATUS) & status::FEATURES_OK, 0);
        w(&t, reg::QUEUE_SEL, 0);
        assert_eq!(r(&t, reg::QUEUE_NUM_MAX), 8);
        w(&t, reg::QUEUE_NUM, 8);
        w(&t, reg::QUEUE_DESC_LOW, drv.q.desc_table.0 as u32);
        w(&t, reg::QUEUE_DESC_HIGH, 0);
        w(&t, reg::QUEUE_DRIVER_LOW, drv.q.avail_ring.0 as u32);
        w(&t, reg::QUEUE_DEVICE_LOW, drv.q.used_ring.0 as u32);
        w(&t, reg::QUEUE_READY, 1);
        w(&t, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK);
        assert_eq!(r(&t, reg::STATUS) & status::DEVICE_NEEDS_RESET, 0);

        let mut cfg = [0u8; 6];
        t.read(reg::CONFIG, &mut cfg);
        assert_eq!(&cfg, b"CONFIG");

        let (head, outs) = drv.add_chain(&[b"ping"], &[4]);
        w(&t, reg::QUEUE_NOTIFY, 0);
        assert_eq!(drv.take_used(), vec![(head, 4)]);
        assert_eq!(drv.read(outs[0], 4), b"ping");
        assert!(chip.level(5));
        assert_eq!(r(&t, reg::INTERRUPT_STATUS), 1);
        w(&t, reg::INTERRUPT_ACK, 1);
        assert!(!chip.level(5));

        // Missing shm region reads as all ones.
        assert_eq!(r(&t, reg::SHM_LEN_LOW), 0xffff_ffff);
        // Reset clears state.
        w(&t, reg::STATUS, 0);
        assert_eq!(r(&t, reg::STATUS), 0);
        assert_eq!(r(&t, reg::QUEUE_READY), 0);
    }

    #[test]
    fn rejects_unknown_features() {
        let drv = Driver::new(8);
        let chip = Arc::new(RecordingIrqChip::default());
        let t = MmioTransport::new(Box::new(Echo { ctx: None }), drv.mem.clone(), VirtioInterrupt::new(IrqLine::new(chip, 0)));
        w(&t, reg::DRIVER_FEATURES_SEL, 0);
        w(&t, reg::DRIVER_FEATURES, 1 << 7); // not offered
        w(&t, reg::DRIVER_FEATURES_SEL, 1);
        w(&t, reg::DRIVER_FEATURES, 1);
        w(&t, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
        assert_eq!(r(&t, reg::STATUS) & status::FEATURES_OK, 0);
    }
}
