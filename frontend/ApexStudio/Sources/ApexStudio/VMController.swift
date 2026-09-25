import CApex
import Foundation

/// Owns the VMM instance (one per process — a Hypervisor.framework rule)
/// and its lifecycle: create → start → wait (background) → destroy.
final class VMController {
    enum StopReason: Int32 {
        case none = 0, powerOff = 1, reset = 2, requested = 3, error = 4
    }

    private(set) var handle: OpaquePointer?
    private let profilePath: String
    private let waitQueue = DispatchQueue(label: "apex.vm.wait", qos: .userInitiated)

    /// Called on the main queue when the guest stops.
    var onStopped: ((StopReason, String?) -> Void)?
    /// Called (on a VMM thread) with console bytes when the profile routes
    /// the console to the app.
    var onSerial: ((Data) -> Void)?

    init(profilePath: String) {
        self.profilePath = profilePath
    }

    static var version: String { String(cString: apex_version()) }

    static var hostCapabilities: String {
        guard let s = apex_host_capabilities() else { return "unknown" }
        defer { apex_string_free(s) }
        return String(cString: s)
    }

    static func installLogger() {
        apex_set_log({ _, level, message in
            guard let message else { return }
            let tag = ["", "E", "W", "I", "D", "T"][Int(max(0, min(5, level)))]
            print("[apex \(tag)] \(String(cString: message))")
        }, nil, 3)
    }

    func boot() throws {
        var hooks = ApexHooks()
        hooks.serial = { ctx, data, len in
            guard let ctx, let data else { return }
            let me = Unmanaged<VMController>.fromOpaque(ctx).takeUnretainedValue()
            me.onSerial?(Data(bytes: data, count: len))
        }
        hooks.serial_ctx = Unmanaged.passUnretained(self).toOpaque()

        var err = [CChar](repeating: 0, count: 1024)
        guard let vm = apex_vm_create(profilePath, &hooks, &err, err.count) else {
            throw NSError(domain: "apex", code: 1, userInfo: [NSLocalizedDescriptionKey: String(cString: err)])
        }
        handle = vm
        guard apex_vm_start(vm) == 0 else {
            let msg = apex_vm_last_error(vm).map { String(cString: $0) } ?? "failed to start vCPUs"
            apex_vm_destroy(vm)
            handle = nil
            throw NSError(domain: "apex", code: 2, userInfo: [NSLocalizedDescriptionKey: msg])
        }
        waitQueue.async { [weak self] in
            let code = apex_vm_wait(vm)
            let message = apex_vm_last_error(vm).map { String(cString: $0) }
            DispatchQueue.main.async {
                self?.onStopped?(StopReason(rawValue: code) ?? .error, message)
            }
        }
    }

    func requestStop() {
        if let h = handle { apex_vm_request_stop(h) }
    }

    /// Tear down after `onStopped` fired. Required before booting again.
    func destroy() {
        if let h = handle {
            apex_vm_destroy(h)
            handle = nil
        }
    }

    var displayInfo: ApexDisplayInfo {
        var info = ApexDisplayInfo()
        if let h = handle { apex_display_info(h, &info) }
        return info
    }

    func stats() -> ApexStats {
        var s = ApexStats()
        if let h = handle { apex_stats(h, &s) }
        return s
    }

    // MARK: input and hardware

    func touch(_ contacts: [ApexTouch]) {
        guard let h = handle else { return }
        contacts.withUnsafeBufferPointer { apex_touch_frame(h, $0.baseAddress, UInt32($0.count)) }
    }

    func key(_ linuxCode: UInt16, down: Bool) {
        if let h = handle { apex_key(h, linuxCode, down) }
    }

    func tap(key linuxCode: UInt16, holdMs: Int = 60) {
        key(linuxCode, down: true)
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(holdMs)) { [weak self] in
            self?.key(linuxCode, down: false)
        }
    }

    func setBattery(percent: Int, charging: Bool, acOnline: Bool) {
        if let h = handle { apex_battery_set(h, UInt32(max(0, min(100, percent))), charging, acOnline) }
    }

    func consoleInput(_ data: Data) {
        guard let h = handle else { return }
        data.withUnsafeBytes { raw in
            apex_console_input(h, raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
    }
}
