import Foundation
import IOKit.ps

/// Mirrors the Mac's own battery (level, charging, AC) into the guest's
/// goldfish battery so Android shows real power state. Desktop Macs report
/// a permanently charged battery on AC power.
final class BatteryMirror {
    private var timer: Timer?
    private weak var controller: VMController?

    init(controller: VMController) {
        self.controller = controller
    }

    func start() {
        update()
        timer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in self?.update() }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    static func read() -> (percent: Int, charging: Bool, ac: Bool) {
        guard let info = IOPSCopyPowerSourcesInfo()?.takeRetainedValue(),
              let list = IOPSCopyPowerSourcesList(info)?.takeRetainedValue() as? [CFTypeRef]
        else { return (100, false, true) }
        for source in list {
            guard let desc = IOPSGetPowerSourceDescription(info, source)?.takeUnretainedValue() as? [String: Any] else { continue }
            let current = desc[kIOPSCurrentCapacityKey] as? Int ?? 100
            let maximum = max(desc[kIOPSMaxCapacityKey] as? Int ?? 100, 1)
            let charging = desc[kIOPSIsChargingKey] as? Bool ?? false
            let ac = (desc[kIOPSPowerSourceStateKey] as? String) == kIOPSACPowerValue
            return (current * 100 / maximum, charging, ac)
        }
        return (100, false, true)
    }

    private func update() {
        let s = BatteryMirror.read()
        controller?.setBattery(percent: s.percent, charging: s.charging, acOnline: s.ac)
    }
}
