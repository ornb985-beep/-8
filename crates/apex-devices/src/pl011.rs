//! ARM PL011 UART (`arm,pl011` / `arm,primecell`), used for `earlycon` and
//! the first kernel messages before virtio-console is up.

use std::collections::VecDeque;
use std::sync::Mutex;

use apex_core::bus::MmioDevice;
use apex_core::irq::IrqLine;
use apex_core::mem::{le_read, le_write};

const DR: u64 = 0x000;
const RSR: u64 = 0x004;
const FR: u64 = 0x018;
const IBRD: u64 = 0x024;
const FBRD: u64 = 0x028;
const LCR_H: u64 = 0x02c;
const CR: u64 = 0x030;
const IFLS: u64 = 0x034;
const IMSC: u64 = 0x038;
const RIS: u64 = 0x03c;
const MIS: u64 = 0x040;
const ICR: u64 = 0x044;
const DMACR: u64 = 0x048;

const FR_RXFE: u32 = 1 << 4;
const FR_TXFE: u32 = 1 << 7;
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;
const FIFO_DEPTH: usize = 32;

const ID: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

struct Regs {
    rx: VecDeque<u8>,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    ris: u32,
    dmacr: u32,
    line: bool,
}

pub type OutputFn = Box<dyn Fn(&[u8]) + Send + Sync>;

pub struct Pl011 {
    regs: Mutex<Regs>,
    irq: IrqLine,
    out: OutputFn,
}

impl Pl011 {
    pub fn new(irq: IrqLine, out: OutputFn) -> Pl011 {
        Pl011 {
            regs: Mutex::new(Regs {
                rx: VecDeque::new(),
                ibrd: 0,
                fbrd: 0,
                lcr_h: 0,
                cr: 0x300,
                ifls: 0x12,
                imsc: 0,
                ris: 0,
                dmacr: 0,
                line: false,
            }),
            irq,
            out,
        }
    }

    fn update(&self, r: &mut Regs) {
        let level = r.ris & r.imsc != 0;
        if level != r.line {
            r.line = level;
            self.irq.set_level(level);
        }
    }

    /// Host keyboard input for the serial console.
    pub fn push_input(&self, data: &[u8]) {
        let mut r = self.regs.lock().unwrap();
        for &b in data {
            if r.rx.len() < FIFO_DEPTH {
                r.rx.push_back(b);
            }
        }
        if !r.rx.is_empty() {
            r.ris |= INT_RX | INT_RT;
        }
        self.update(&mut r);
    }
}

impl MmioDevice for Pl011 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut r = self.regs.lock().unwrap();
        let v: u32 = match offset {
            DR => {
                let c = r.rx.pop_front().unwrap_or(0) as u32;
                if r.rx.is_empty() {
                    r.ris &= !(INT_RX | INT_RT);
                }
                self.update(&mut r);
                c
            }
            RSR => 0,
            FR => FR_TXFE | if r.rx.is_empty() { FR_RXFE } else { 0 },
            IBRD => r.ibrd,
            FBRD => r.fbrd,
            LCR_H => r.lcr_h,
            CR => r.cr,
            IFLS => r.ifls,
            IMSC => r.imsc,
            RIS => r.ris,
            MIS => r.ris & r.imsc,
            DMACR => r.dmacr,
            0xfe0..=0xffc => ID[((offset - 0xfe0) / 4) as usize] as u32,
            _ => 0,
        };
        le_write(data, v as u64);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_read(data) as u32;
        let mut r = self.regs.lock().unwrap();
        match offset {
            DR => {
                (self.out)(&[v as u8]);
                r.ris |= INT_TX;
            }
            RSR => {}
            IBRD => r.ibrd = v & 0xffff,
            FBRD => r.fbrd = v & 0x3f,
            LCR_H => r.lcr_h = v & 0xff,
            CR => r.cr = v & 0xffff,
            IFLS => r.ifls = v & 0x3f,
            IMSC => r.imsc = v & 0x7ff,
            ICR => r.ris &= !v,
            DMACR => r.dmacr = v & 7,
            _ => {}
        }
        self.update(&mut r);
    }

    fn name(&self) -> &str {
        "pl011"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::irq::RecordingIrqChip;
    use std::sync::Arc;

    #[test]
    fn tx_rx_and_ids() {
        let chip = Arc::new(RecordingIrqChip::default());
        let out = Arc::new(Mutex::new(Vec::new()));
        let o = out.clone();
        let u = Pl011::new(IrqLine::new(chip.clone(), 1), Box::new(move |b| o.lock().unwrap().extend_from_slice(b)));
        for c in b"OK" {
            u.write(DR, &[*c]);
        }
        assert_eq!(out.lock().unwrap().as_slice(), b"OK");
        let mut b = [0u8; 4];
        u.read(0xfe8, &mut b);
        assert_eq!(b[0], 0x14);
        u.read(0xff4, &mut b);
        assert_eq!(b[0], 0xf0);
        u.write(IMSC, &INT_RX.to_le_bytes());
        u.push_input(b"x");
        assert!(chip.level(1));
        u.read(FR, &mut b);
        assert_eq!(u32::from_le_bytes(b) & FR_RXFE, 0);
        u.read(DR, &mut b);
        assert_eq!(b[0], b'x');
        assert!(!chip.level(1));
    }
}
