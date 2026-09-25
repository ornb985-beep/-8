//! ARM PL031 real time clock. Android reads wall-clock time from
//! /dev/rtc0 at boot; the alarm interrupt backs `RTC_WKALM_SET`.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apex_core::bus::MmioDevice;
use apex_core::irq::IrqLine;
use apex_core::mem::{le_read, le_write};

const DR: u64 = 0x00;
const MR: u64 = 0x04;
const LR: u64 = 0x08;
const CR: u64 = 0x0c;
const IMSC: u64 = 0x10;
const RIS: u64 = 0x14;
const MIS: u64 = 0x18;
const ICR: u64 = 0x1c;

const ID: [u8; 8] = [0x31, 0x10, 0x04, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

struct Regs {
    /// Guest time = host time + offset (seconds).
    offset: i64,
    mr: u32,
    imsc: u32,
    ris: u32,
    alarm_gen: u64,
}

pub struct Pl031 {
    regs: Arc<Mutex<Regs>>,
    irq: IrqLine,
}

fn host_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Pl031 {
    pub fn new(irq: IrqLine) -> Pl031 {
        Pl031 { regs: Arc::new(Mutex::new(Regs { offset: 0, mr: 0, imsc: 0, ris: 0, alarm_gen: 0 })), irq }
    }

    fn now(r: &Regs) -> u32 {
        (host_now() + r.offset) as u32
    }

    fn update(&self, r: &Regs) {
        self.irq.set_level(r.ris & r.imsc & 1 != 0);
    }

    /// Arm a one-shot host timer for the match register.
    fn arm(&self, r: &mut Regs) {
        r.alarm_gen += 1;
        if r.imsc & 1 == 0 {
            return;
        }
        let now = Self::now(r);
        let delta = r.mr.wrapping_sub(now) as i32;
        if delta <= 0 {
            return;
        }
        let gen = r.alarm_gen;
        let regs = self.regs.clone();
        let irq = self.irq.clone();
        let target = r.mr;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(delta as u64));
            let mut r = regs.lock().unwrap();
            if r.alarm_gen == gen && Self::now(&r) >= target {
                r.ris |= 1;
                irq.set_level(r.ris & r.imsc & 1 != 0);
            }
        });
    }
}

impl MmioDevice for Pl031 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let r = self.regs.lock().unwrap();
        let v = match offset {
            DR => Pl031::now(&r),
            MR => r.mr,
            LR => Pl031::now(&r),
            CR => 1,
            IMSC => r.imsc,
            RIS => r.ris,
            MIS => r.ris & r.imsc,
            0xfe0..=0xffc => ID[((offset - 0xfe0) / 4) as usize] as u32,
            _ => 0,
        };
        le_write(data, v as u64);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_read(data) as u32;
        let mut r = self.regs.lock().unwrap();
        match offset {
            MR => {
                r.mr = v;
                self.arm(&mut r);
            }
            LR => r.offset = v as i64 - host_now(),
            IMSC => {
                r.imsc = v & 1;
                self.arm(&mut r);
            }
            ICR => r.ris &= !v,
            _ => {}
        }
        self.update(&r);
    }

    fn name(&self) -> &str {
        "pl031"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::irq::RecordingIrqChip;

    #[test]
    fn reads_host_time_and_allows_set() {
        let chip = Arc::new(RecordingIrqChip::default());
        let rtc = Pl031::new(IrqLine::new(chip, 2));
        let mut b = [0u8; 4];
        rtc.read(DR, &mut b);
        let t = u32::from_le_bytes(b) as i64;
        assert!((t - host_now()).abs() <= 1);
        rtc.write(LR, &1_000_000u32.to_le_bytes());
        rtc.read(DR, &mut b);
        assert!((u32::from_le_bytes(b) as i64 - 1_000_000).abs() <= 1);
        rtc.read(0xfe0, &mut b);
        assert_eq!(b[0], 0x31);
    }
}
