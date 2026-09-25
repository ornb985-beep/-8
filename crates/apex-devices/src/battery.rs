//! goldfish battery (`google,goldfish-battery`, CONFIG_BATTERY_GOLDFISH).
//! Gives Android a real power_supply: level, charging state, health,
//! voltage, temperature. The macOS frontend can mirror the MacBook's own
//! battery into the guest.

use std::sync::Mutex;

use apex_core::bus::MmioDevice;
use apex_core::irq::IrqLine;
use apex_core::mem::le_write;

const INT_STATUS: u64 = 0x00;
const INT_ENABLE: u64 = 0x04;
const AC_ONLINE: u64 = 0x08;
const STATUS: u64 = 0x0c;
const HEALTH: u64 = 0x10;
const PRESENT: u64 = 0x14;
const CAPACITY: u64 = 0x18;
const VOLTAGE: u64 = 0x1c;
const TEMP: u64 = 0x20;
const CHARGE_COUNTER: u64 = 0x24;
const VOLTAGE_MAX: u64 = 0x28;
const CURRENT_MAX: u64 = 0x2c;
const CURRENT_NOW: u64 = 0x30;
const CURRENT_AVG: u64 = 0x34;
const CHARGE_FULL_UAH: u64 = 0x38;
const CYCLE_COUNT: u64 = 0x40;

const BATTERY_STATUS_CHANGED: u32 = 1;
const AC_STATUS_CHANGED: u32 = 2;

/// POWER_SUPPLY_STATUS_*
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ChargeStatus {
    Unknown = 0,
    Charging = 1,
    Discharging = 2,
    NotCharging = 3,
    Full = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatteryState {
    pub ac_online: bool,
    pub status: ChargeStatus,
    /// 0..=100
    pub capacity: u32,
    /// microvolts
    pub voltage_uv: u32,
    /// tenths of a degree Celsius
    pub temp_decic: i32,
    /// microamps (negative = discharging)
    pub current_ua: i32,
    pub charge_full_uah: u32,
    pub cycle_count: u32,
}

impl Default for BatteryState {
    fn default() -> Self {
        BatteryState {
            ac_online: true,
            status: ChargeStatus::Charging,
            capacity: 87,
            voltage_uv: 4_150_000,
            temp_decic: 290,
            current_ua: 900_000,
            charge_full_uah: 4_500_000,
            cycle_count: 42,
        }
    }
}

struct Regs {
    state: BatteryState,
    int_status: u32,
    int_enable: u32,
}

pub struct GoldfishBattery {
    regs: Mutex<Regs>,
    irq: IrqLine,
}

impl GoldfishBattery {
    pub fn new(irq: IrqLine) -> GoldfishBattery {
        GoldfishBattery { regs: Mutex::new(Regs { state: BatteryState::default(), int_status: 0, int_enable: 0 }), irq }
    }

    pub fn state(&self) -> BatteryState {
        self.regs.lock().unwrap().state
    }

    /// Update from the host and notify the guest's power_supply core.
    pub fn set_state(&self, s: BatteryState) {
        let mut r = self.regs.lock().unwrap();
        let old = r.state;
        r.state = s;
        if old.ac_online != s.ac_online {
            r.int_status |= AC_STATUS_CHANGED;
        }
        if old != s {
            r.int_status |= BATTERY_STATUS_CHANGED;
        }
        self.irq.set_level(r.int_status & r.int_enable != 0);
    }
}

impl MmioDevice for GoldfishBattery {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut r = self.regs.lock().unwrap();
        let s = r.state;
        let v: u32 = match offset {
            INT_STATUS => {
                // Read-to-clear, as the goldfish driver's IRQ handler expects.
                let v = r.int_status & r.int_enable;
                r.int_status = 0;
                self.irq.set_level(false);
                v
            }
            INT_ENABLE => r.int_enable,
            AC_ONLINE => s.ac_online as u32,
            STATUS => s.status as u32,
            HEALTH => 1, // POWER_SUPPLY_HEALTH_GOOD
            PRESENT => 1,
            CAPACITY => s.capacity.min(100),
            VOLTAGE => s.voltage_uv,
            TEMP => s.temp_decic as u32,
            CHARGE_COUNTER => (s.charge_full_uah as u64 * s.capacity.min(100) as u64 / 100) as u32,
            VOLTAGE_MAX => 4_400_000,
            CURRENT_MAX => 3_000_000,
            CURRENT_NOW | CURRENT_AVG => s.current_ua as u32,
            CHARGE_FULL_UAH => s.charge_full_uah,
            CYCLE_COUNT => s.cycle_count,
            _ => 0,
        };
        le_write(data, v as u64);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset == INT_ENABLE {
            let mut r = self.regs.lock().unwrap();
            r.int_enable = apex_core::mem::le_read(data) as u32 & (BATTERY_STATUS_CHANGED | AC_STATUS_CHANGED);
            self.irq.set_level(r.int_status & r.int_enable != 0);
        }
    }

    fn name(&self) -> &str {
        "goldfish-battery"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::irq::RecordingIrqChip;
    use std::sync::Arc;

    #[test]
    fn state_changes_interrupt() {
        let chip = Arc::new(RecordingIrqChip::default());
        let b = GoldfishBattery::new(IrqLine::new(chip.clone(), 3));
        b.write(INT_ENABLE, &3u32.to_le_bytes());
        let mut v = [0u8; 4];
        b.read(CAPACITY, &mut v);
        assert_eq!(u32::from_le_bytes(v), 87);
        b.set_state(BatteryState { ac_online: false, status: ChargeStatus::Discharging, capacity: 50, ..Default::default() });
        assert!(chip.level(3));
        b.read(INT_STATUS, &mut v);
        assert_eq!(u32::from_le_bytes(v), 3);
        assert!(!chip.level(3));
        b.read(STATUS, &mut v);
        assert_eq!(u32::from_le_bytes(v), 2);
    }
}
