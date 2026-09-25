//! virtio-input: touchscreen, keyboard/hardware buttons.
//!
//! Host events are written straight into the guest's event buffers on the
//! calling thread and the interrupt is raised immediately, so the latency
//! from `NSEvent` to the guest evdev node is one memcpy plus one SPI.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use apex_core::mem::GuestMemory;
use apex_core::Result;

use super::{device_type, ActivateContext, Queue, VirtioDevice, VirtioInterrupt};
use crate::input::{abs, ev, key, prop, Contact, InputEvent, TouchTracker, BUS_VIRTUAL};

const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

const EVENTQ: usize = 0;
const STATUSQ: usize = 1;
const MAX_PENDING: usize = 8192;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub res: i32,
}

/// Static description of an input device (what evdev will report).
#[derive(Clone, Debug)]
pub struct InputSpec {
    pub name: String,
    pub serial: String,
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub props: Vec<u16>,
    pub events: BTreeMap<u16, Vec<u16>>,
    pub abs: BTreeMap<u16, AbsInfo>,
}

impl InputSpec {
    /// A phone touchscreen with `slots` simultaneous contacts.
    pub fn touchscreen(width: u32, height: u32, slots: u32, dpi: u32) -> InputSpec {
        // Resolution in units/mm lets Android compute physical sizes.
        let res = (dpi as f64 / 25.4).round() as i32;
        let mut abs_map = BTreeMap::new();
        let axis = |max: u32| AbsInfo { min: 0, max: max as i32 - 1, fuzz: 0, flat: 0, res };
        abs_map.insert(abs::X, axis(width));
        abs_map.insert(abs::Y, axis(height));
        abs_map.insert(abs::MT_SLOT, AbsInfo { max: slots as i32 - 1, ..Default::default() });
        abs_map.insert(abs::MT_TOUCH_MAJOR, AbsInfo { max: 255, ..Default::default() });
        abs_map.insert(abs::MT_POSITION_X, axis(width));
        abs_map.insert(abs::MT_POSITION_Y, axis(height));
        abs_map.insert(abs::MT_TOOL_TYPE, AbsInfo { max: 2, ..Default::default() });
        abs_map.insert(abs::MT_TRACKING_ID, AbsInfo { max: 0xffff, ..Default::default() });
        abs_map.insert(abs::MT_PRESSURE, AbsInfo { max: 255, ..Default::default() });
        let mut events = BTreeMap::new();
        events.insert(ev::KEY, vec![key::BTN_TOOL_FINGER, key::BTN_TOUCH]);
        events.insert(ev::ABS, abs_map.keys().copied().collect());
        InputSpec {
            name: "Apex Touchscreen".into(),
            serial: "apex-touch-0".into(),
            bustype: BUS_VIRTUAL,
            vendor: 0x1d6b,
            product: 0xa001,
            version: 1,
            props: vec![prop::DIRECT],
            events,
            abs: abs_map,
        }
    }

    /// Hardware buttons + a full keyboard.
    pub fn keyboard() -> InputSpec {
        let mut keys: Vec<u16> = (1..=248).collect();
        keys.extend([key::APPSELECT, key::HOMEPAGE, key::BACK, key::MENU, key::SEARCH, key::CAMERA, key::WAKEUP, key::SLEEP]);
        keys.sort_unstable();
        keys.dedup();
        let mut events = BTreeMap::new();
        events.insert(ev::KEY, keys);
        events.insert(ev::REP, vec![0, 1]);
        InputSpec {
            name: "Apex Keys".into(),
            serial: "apex-keys-0".into(),
            bustype: BUS_VIRTUAL,
            vendor: 0x1d6b,
            product: 0xa002,
            version: 1,
            props: vec![],
            events,
            abs: BTreeMap::new(),
        }
    }

    fn bitmap(codes: &[u16]) -> Vec<u8> {
        let max = codes.iter().copied().max().map(|m| m as usize).unwrap_or(0);
        if codes.is_empty() {
            return Vec::new();
        }
        let mut b = vec![0u8; max / 8 + 1];
        for &c in codes {
            b[c as usize / 8] |= 1 << (c % 8);
        }
        b
    }

    /// Config payload for (select, subsel).
    fn query(&self, select: u8, subsel: u8) -> Vec<u8> {
        match select {
            CFG_ID_NAME => self.name.as_bytes().iter().take(128).copied().collect(),
            CFG_ID_SERIAL => self.serial.as_bytes().iter().take(128).copied().collect(),
            CFG_ID_DEVIDS => {
                let mut v = Vec::new();
                for x in [self.bustype, self.vendor, self.product, self.version] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            CFG_PROP_BITS => Self::bitmap(&self.props),
            CFG_EV_BITS => self.events.get(&(subsel as u16)).map(|c| Self::bitmap(c)).unwrap_or_default(),
            CFG_ABS_INFO => match self.abs.get(&(subsel as u16)) {
                Some(a) => {
                    let mut v = Vec::new();
                    for x in [a.min, a.max, a.fuzz, a.flat, a.res] {
                        v.extend_from_slice(&x.to_le_bytes());
                    }
                    v
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

struct Active {
    mem: GuestMemory,
    queues: Vec<Queue>,
    irq: VirtioInterrupt,
}

struct Shared {
    active: Mutex<Option<Active>>,
    pending: Mutex<VecDeque<InputEvent>>,
}

impl Shared {
    fn flush(&self) {
        let mut a = self.active.lock().unwrap();
        let Some(act) = a.as_mut() else { return };
        let mut pending = self.pending.lock().unwrap();
        let q = &mut act.queues[EVENTQ];
        let mut any = false;
        while let Some(e) = pending.front().copied() {
            let Some(chain) = q.pop(&act.mem) else { break };
            let n = chain.writer(&act.mem).write(&e.to_bytes());
            let _ = q.add_used(&act.mem, chain.head, n as u32);
            pending.pop_front();
            any = true;
        }
        if any && q.needs_notification(&act.mem) {
            act.irq.signal_used_queue();
        }
    }
}

/// Host-side handle for injecting events.
#[derive(Clone)]
pub struct InputHandle {
    shared: Arc<Shared>,
    touch: Option<Arc<Mutex<TouchTracker>>>,
}

impl InputHandle {
    /// Queue raw events (caller includes SYN_REPORT).
    pub fn send(&self, events: &[InputEvent]) {
        if events.is_empty() {
            return;
        }
        {
            let mut p = self.shared.pending.lock().unwrap();
            if p.len() + events.len() > MAX_PENDING {
                // Guest is not draining (driver not bound yet): drop input
                // rather than growing without bound.
                return;
            }
            p.extend(events.iter().copied());
        }
        self.shared.flush();
    }

    /// Full touch frame (all fingers currently down).
    pub fn touch_frame(&self, contacts: &[Contact]) {
        if let Some(t) = &self.touch {
            let events = t.lock().unwrap().update(contacts);
            self.send(&events);
        }
    }

    pub fn key(&self, code: u16, down: bool) {
        self.send(&[InputEvent::new(ev::KEY, code, down as i32), InputEvent::syn()]);
    }
}

pub struct Input {
    spec: InputSpec,
    select: u8,
    subsel: u8,
    shared: Arc<Shared>,
    touch: Option<Arc<Mutex<TouchTracker>>>,
}

impl Input {
    pub fn new(spec: InputSpec) -> Input {
        let touch = spec.abs.get(&abs::MT_SLOT).map(|slot| {
            let w = spec.abs[&abs::MT_POSITION_X].max as u32 + 1;
            let h = spec.abs[&abs::MT_POSITION_Y].max as u32 + 1;
            Arc::new(Mutex::new(TouchTracker::new(slot.max as usize + 1, w, h)))
        });
        Input {
            spec,
            select: 0,
            subsel: 0,
            shared: Arc::new(Shared { active: Mutex::new(None), pending: Mutex::new(VecDeque::new()) }),
            touch,
        }
    }

    pub fn handle(&self) -> InputHandle {
        InputHandle { shared: self.shared.clone(), touch: self.touch.clone() }
    }
}

impl VirtioDevice for Input {
    fn device_type(&self) -> u32 {
        device_type::INPUT
    }
    fn name(&self) -> &str {
        "input"
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 64]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let payload = self.spec.query(self.select, self.subsel);
        let mut cfg = [0u8; 136];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        cfg[2] = payload.len().min(128) as u8;
        cfg[8..8 + payload.len().min(128)].copy_from_slice(&payload[..payload.len().min(128)]);
        super::read_config_bytes(&cfg, offset, data)
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        *self.shared.active.lock().unwrap() = Some(Active { mem: ctx.mem, queues: ctx.queues, irq: ctx.interrupt });
        self.shared.flush();
        Ok(())
    }
    fn queue_notify(&mut self, index: u16) {
        match index as usize {
            EVENTQ => self.shared.flush(),
            STATUSQ => {
                let mut a = self.shared.active.lock().unwrap();
                let Some(act) = a.as_mut() else { return };
                let q = &mut act.queues[STATUSQ];
                let mut any = false;
                while let Some(chain) = q.pop(&act.mem) {
                    // LED / force feedback status from the guest: accepted and ignored.
                    let _ = q.add_used(&act.mem, chain.head, 0);
                    any = true;
                }
                if any && q.needs_notification(&act.mem) {
                    act.irq.signal_used_queue();
                }
            }
            _ => {}
        }
    }
    fn reset(&mut self) {
        *self.shared.active.lock().unwrap() = None;
        self.shared.pending.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::test_driver::Driver;
    use apex_core::irq::{IrqLine, RecordingIrqChip};

    fn cfg_read(dev: &mut Input, select: u8, subsel: u8) -> (u8, Vec<u8>) {
        dev.write_config(0, &[select, subsel]);
        let mut size = [0u8; 1];
        dev.read_config(2, &mut size);
        let mut data = vec![0u8; size[0] as usize];
        dev.read_config(8, &mut data);
        (size[0], data)
    }

    #[test]
    fn touchscreen_config() {
        let mut d = Input::new(InputSpec::touchscreen(1080, 2400, 10, 420));
        let (_, name) = cfg_read(&mut d, CFG_ID_NAME, 0);
        assert_eq!(name, b"Apex Touchscreen");
        let (_, props) = cfg_read(&mut d, CFG_PROP_BITS, 0);
        assert_eq!(props, vec![0b10]); // INPUT_PROP_DIRECT
        let (_, absbits) = cfg_read(&mut d, CFG_EV_BITS, ev::ABS as u8);
        assert_ne!(absbits[abs::MT_POSITION_X as usize / 8] & (1 << (abs::MT_POSITION_X % 8)), 0);
        let (n, info) = cfg_read(&mut d, CFG_ABS_INFO, abs::MT_POSITION_Y as u8);
        assert_eq!(n, 20);
        assert_eq!(i32::from_le_bytes(info[4..8].try_into().unwrap()), 2399);
        assert_eq!(i32::from_le_bytes(info[16..20].try_into().unwrap()), 17); // 420 dpi ~ 16.5 dots/mm
        let (n, _) = cfg_read(&mut d, CFG_EV_BITS, ev::REL as u8);
        assert_eq!(n, 0);
        let (_, ids) = cfg_read(&mut d, CFG_ID_DEVIDS, 0);
        assert_eq!(u16::from_le_bytes([ids[0], ids[1]]), BUS_VIRTUAL);
    }

    #[test]
    fn events_flow_into_buffers() {
        let mut drv = Driver::new(64);
        let mut d = Input::new(InputSpec::touchscreen(100, 100, 4, 160));
        let h = d.handle();
        let bufs: Vec<_> = (0..32).map(|_| drv.add_chain(&[], &[8]).1[0]).collect();
        let chip = Arc::new(RecordingIrqChip::default());
        let mut statusq = drv.q.clone();
        statusq.ready = false;
        d.activate(ActivateContext {
            mem: drv.mem.clone(),
            queues: vec![drv.q.clone(), statusq],
            interrupt: VirtioInterrupt::new(IrqLine::new(chip.clone(), 3)),
            features: 0,
        })
        .unwrap();
        h.touch_frame(&[Contact { id: 1, x: 10, y: 20, pressure: 30, major: 4 }]);
        let used = drv.take_used();
        assert!(used.len() >= 6);
        assert!(chip.level(3));
        let first = drv.read(bufs[0], 8);
        assert_eq!(u16::from_le_bytes([first[0], first[1]]), ev::ABS);
        assert_eq!(u16::from_le_bytes([first[2], first[3]]), abs::MT_SLOT);
        let last = drv.read(bufs[used.len() - 1], 8);
        assert_eq!(&last, &InputEvent::syn().to_bytes());
    }
}
