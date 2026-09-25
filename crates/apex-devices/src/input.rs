//! Linux evdev codes and the multitouch (MT protocol B) state machine that
//! turns host pointer/trackpad contacts into a real phone touchscreen event
//! stream.

pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
    pub const MSC: u16 = 0x04;
    pub const SW: u16 = 0x05;
    pub const LED: u16 = 0x11;
    pub const REP: u16 = 0x14;
}

pub const SYN_REPORT: u16 = 0;

pub mod abs {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
    pub const MT_SLOT: u16 = 0x2f;
    pub const MT_TOUCH_MAJOR: u16 = 0x30;
    pub const MT_TOUCH_MINOR: u16 = 0x31;
    pub const MT_ORIENTATION: u16 = 0x34;
    pub const MT_POSITION_X: u16 = 0x35;
    pub const MT_POSITION_Y: u16 = 0x36;
    pub const MT_TOOL_TYPE: u16 = 0x37;
    pub const MT_TRACKING_ID: u16 = 0x39;
    pub const MT_PRESSURE: u16 = 0x3a;
}

pub mod key {
    pub const ESC: u16 = 1;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const ENTER: u16 = 28;
    pub const LEFTCTRL: u16 = 29;
    pub const LEFTSHIFT: u16 = 42;
    pub const RIGHTSHIFT: u16 = 54;
    pub const LEFTALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const CAPSLOCK: u16 = 58;
    pub const UP: u16 = 103;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const DOWN: u16 = 108;
    pub const DELETE: u16 = 111;
    pub const MUTE: u16 = 113;
    pub const VOLUMEDOWN: u16 = 114;
    pub const VOLUMEUP: u16 = 115;
    pub const POWER: u16 = 116;
    pub const LEFTMETA: u16 = 125;
    pub const MENU: u16 = 139;
    pub const SLEEP: u16 = 142;
    pub const WAKEUP: u16 = 143;
    pub const BACK: u16 = 158;
    pub const HOMEPAGE: u16 = 172;
    pub const CAMERA: u16 = 212;
    pub const SEARCH: u16 = 217;
    pub const APPSELECT: u16 = 0x244;
    pub const BTN_TOOL_FINGER: u16 = 0x145;
    pub const BTN_TOUCH: u16 = 0x14a;
}

pub mod prop {
    pub const POINTER: u16 = 0;
    pub const DIRECT: u16 = 1;
}

pub const BUS_VIRTUAL: u16 = 0x06;
pub const MT_TOOL_FINGER: i32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const fn new(kind: u16, code: u16, value: i32) -> InputEvent {
        InputEvent { kind, code, value }
    }
    pub const fn syn() -> InputEvent {
        InputEvent::new(ev::SYN, SYN_REPORT, 0)
    }
    pub fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

/// A finger on the screen, in guest display pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contact {
    /// Host-assigned identifier, stable while the finger stays down.
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub pressure: i32,
    pub major: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    host_id: Option<u32>,
    tracking_id: i32,
    x: i32,
    y: i32,
    pressure: i32,
    major: i32,
}

/// MT protocol B encoder with slot allocation.
pub struct TouchTracker {
    slots: Vec<Slot>,
    next_tracking_id: i32,
    current_slot: i32,
    touching: bool,
    st_pos: (i32, i32),
    width: i32,
    height: i32,
}

impl TouchTracker {
    pub fn new(max_slots: usize, width: u32, height: u32) -> TouchTracker {
        TouchTracker {
            slots: vec![Slot::default(); max_slots],
            next_tracking_id: 0,
            current_slot: -1,
            touching: false,
            st_pos: (-1, -1),
            width: width as i32,
            height: height as i32,
        }
    }

    pub fn max_slots(&self) -> usize {
        self.slots.len()
    }

    /// Replace the full set of contacts; returns the events to send
    /// (terminated by SYN_REPORT), or nothing if the frame changed nothing.
    pub fn update(&mut self, contacts: &[Contact]) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let (w, h) = (self.width, self.height);
        let clamp_x = move |v: i32| v.clamp(0, w - 1);
        let clamp_y = move |v: i32| v.clamp(0, h - 1);

        // Lift fingers that disappeared.
        for s in 0..self.slots.len() {
            if let Some(hid) = self.slots[s].host_id {
                if !contacts.iter().any(|c| c.id == hid) {
                    self.select(s, &mut out);
                    out.push(InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, -1));
                    self.slots[s].host_id = None;
                }
            }
        }
        for c in contacts {
            let (x, y) = (clamp_x(c.x), clamp_y(c.y));
            let existing = self.slots.iter().position(|s| s.host_id == Some(c.id));
            let s = match existing {
                Some(s) => s,
                None => {
                    let Some(free) = self.slots.iter().position(|s| s.host_id.is_none()) else { continue };
                    self.select(free, &mut out);
                    let tid = self.next_tracking_id;
                    self.next_tracking_id = (self.next_tracking_id + 1) & 0xffff;
                    out.push(InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, tid));
                    out.push(InputEvent::new(ev::ABS, abs::MT_TOOL_TYPE, MT_TOOL_FINGER));
                    self.slots[free] = Slot { host_id: Some(c.id), tracking_id: tid, x: -1, y: -1, pressure: -1, major: -1 };
                    free
                }
            };
            let slot = self.slots[s];
            if slot.x != x {
                self.select(s, &mut out);
                out.push(InputEvent::new(ev::ABS, abs::MT_POSITION_X, x));
            }
            if slot.y != y {
                self.select(s, &mut out);
                out.push(InputEvent::new(ev::ABS, abs::MT_POSITION_Y, y));
            }
            if slot.pressure != c.pressure {
                self.select(s, &mut out);
                out.push(InputEvent::new(ev::ABS, abs::MT_PRESSURE, c.pressure));
            }
            if slot.major != c.major {
                self.select(s, &mut out);
                out.push(InputEvent::new(ev::ABS, abs::MT_TOUCH_MAJOR, c.major));
            }
            let sl = &mut self.slots[s];
            sl.x = x;
            sl.y = y;
            sl.pressure = c.pressure;
            sl.major = c.major;
        }
        let touching = self.slots.iter().any(|s| s.host_id.is_some());
        if touching != self.touching {
            out.push(InputEvent::new(ev::KEY, key::BTN_TOUCH, touching as i32));
            out.push(InputEvent::new(ev::KEY, key::BTN_TOOL_FINGER, touching as i32));
            self.touching = touching;
        }
        // Single-touch emulation for legacy consumers.
        if let Some(first) = self.slots.iter().find(|s| s.host_id.is_some()).copied() {
            if first.x != self.st_pos.0 {
                out.push(InputEvent::new(ev::ABS, abs::X, first.x));
            }
            if first.y != self.st_pos.1 {
                out.push(InputEvent::new(ev::ABS, abs::Y, first.y));
            }
            self.st_pos = (first.x, first.y);
        }
        if out.is_empty() {
            return out;
        }
        out.push(InputEvent::syn());
        out
    }

    fn select(&mut self, s: usize, out: &mut Vec<InputEvent>) {
        if self.current_slot != s as i32 {
            out.push(InputEvent::new(ev::ABS, abs::MT_SLOT, s as i32));
            self.current_slot = s as i32;
        }
    }
}

/// macOS virtual key code (kVK_*) to Linux evdev key code.
pub fn mac_keycode_to_linux(vk: u16) -> Option<u16> {
    Some(match vk {
        0x00 => 30, // A
        0x01 => 31, // S
        0x02 => 32, // D
        0x03 => 33, // F
        0x04 => 35, // H
        0x05 => 34, // G
        0x06 => 44, // Z
        0x07 => 45, // X
        0x08 => 46, // C
        0x09 => 47, // V
        0x0b => 48, // B
        0x0c => 16, // Q
        0x0d => 17, // W
        0x0e => 18, // E
        0x0f => 19, // R
        0x10 => 21, // Y
        0x11 => 20, // T
        0x12 => 2,  // 1
        0x13 => 3,  // 2
        0x14 => 4,  // 3
        0x15 => 5,  // 4
        0x16 => 7,  // 6
        0x17 => 6,  // 5
        0x18 => 13, // =
        0x19 => 10, // 9
        0x1a => 8,  // 7
        0x1b => 12, // -
        0x1c => 9,  // 8
        0x1d => 11, // 0
        0x1e => 27, // ]
        0x1f => 24, // O
        0x20 => 22, // U
        0x21 => 26, // [
        0x22 => 23, // I
        0x23 => 25, // P
        0x24 => key::ENTER,
        0x25 => 38, // L
        0x26 => 36, // J
        0x27 => 40, // '
        0x28 => 37, // K
        0x29 => 39, // ;
        0x2a => 43, // backslash
        0x2b => 51, // ,
        0x2c => 53, // /
        0x2d => 49, // N
        0x2e => 50, // M
        0x2f => 52, // .
        0x30 => key::TAB,
        0x31 => key::SPACE,
        0x32 => 41, // `
        0x33 => key::BACKSPACE,
        0x35 => key::ESC,
        0x37 => key::LEFTMETA,
        0x38 => key::LEFTSHIFT,
        0x39 => key::CAPSLOCK,
        0x3a => key::LEFTALT,
        0x3b => key::LEFTCTRL,
        0x3c => key::RIGHTSHIFT,
        0x3d => 100, // right alt
        0x3e => 97,  // right ctrl
        0x48 => key::VOLUMEUP,
        0x49 => key::VOLUMEDOWN,
        0x4a => key::MUTE,
        0x75 => key::DELETE,
        0x7b => key::LEFT,
        0x7c => key::RIGHT,
        0x7d => key::DOWN,
        0x7e => key::UP,
        0x7a => 59,  // F1
        0x78 => 60,  // F2
        0x63 => 61,  // F3
        0x76 => 62,  // F4
        0x60 => 63,  // F5
        0x61 => 64,  // F6
        0x62 => 65,  // F7
        0x64 => 66,  // F8
        0x65 => 67,  // F9
        0x6d => 68,  // F10
        0x67 => 87,  // F11
        0x6f => 88,  // F12
        0x73 => 102, // Home
        0x77 => 107, // End
        0x74 => 104, // PageUp
        0x79 => 109, // PageDown
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u32, x: i32, y: i32) -> Contact {
        Contact { id, x, y, pressure: 50, major: 8 }
    }

    #[test]
    fn single_tap_sequence() {
        let mut t = TouchTracker::new(10, 1080, 2400);
        let down = t.update(&[c(7, 100, 200)]);
        assert_eq!(down[0], InputEvent::new(ev::ABS, abs::MT_SLOT, 0));
        assert_eq!(down[1], InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, 0));
        assert!(down.contains(&InputEvent::new(ev::ABS, abs::MT_POSITION_X, 100)));
        assert!(down.contains(&InputEvent::new(ev::KEY, key::BTN_TOUCH, 1)));
        assert_eq!(*down.last().unwrap(), InputEvent::syn());
        // Unchanged frame produces no events at all.
        assert!(t.update(&[c(7, 100, 200)]).is_empty());
        let mv = t.update(&[c(7, 5000, 210)]); // clamped to width-1
        assert!(mv.contains(&InputEvent::new(ev::ABS, abs::MT_POSITION_X, 1079)));
        assert!(!mv.contains(&InputEvent::new(ev::ABS, abs::MT_SLOT, 0)), "slot already selected");
        let up = t.update(&[]);
        assert!(up.contains(&InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, -1)));
        assert!(up.contains(&InputEvent::new(ev::KEY, key::BTN_TOUCH, 0)));
    }

    #[test]
    fn two_finger_pinch_uses_two_slots() {
        let mut t = TouchTracker::new(10, 1080, 2400);
        let e = t.update(&[c(1, 400, 1000), c(2, 600, 1400)]);
        assert!(e.contains(&InputEvent::new(ev::ABS, abs::MT_SLOT, 1)));
        assert!(e.contains(&InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, 1)));
        let e = t.update(&[c(2, 650, 1450)]);
        // finger 1 lifted from slot 0, finger 2 moved in slot 1
        let lift = e.iter().position(|x| *x == InputEvent::new(ev::ABS, abs::MT_TRACKING_ID, -1)).unwrap();
        assert_eq!(e[lift - 1], InputEvent::new(ev::ABS, abs::MT_SLOT, 0));
        assert!(e.contains(&InputEvent::new(ev::ABS, abs::MT_POSITION_X, 650)));
        assert!(!e.contains(&InputEvent::new(ev::KEY, key::BTN_TOUCH, 0)));
    }

    #[test]
    fn slot_exhaustion_ignores_extra_fingers() {
        let mut t = TouchTracker::new(2, 100, 100);
        let e = t.update(&[c(1, 1, 1), c(2, 2, 2), c(3, 3, 3)]);
        assert_eq!(e.iter().filter(|x| x.code == abs::MT_TRACKING_ID).count(), 2);
    }

    #[test]
    fn keymap() {
        assert_eq!(mac_keycode_to_linux(0x00), Some(30));
        assert_eq!(mac_keycode_to_linux(0x24), Some(key::ENTER));
        assert_eq!(mac_keycode_to_linux(0xff), None);
    }
}
