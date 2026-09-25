import AppKit
import CApex

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var window: NSWindow?
    private var phoneView: PhoneView?
    private var renderer: Renderer?
    private var controller: VMController?
    private var battery: BatteryMirror?
    private var hud: NSTextField?
    private var hudTimer: Timer?
    private var lastCounters: (presented: UInt64, fresh: UInt64, submitted: UInt64) = (0, 0, 0)
    private var profilePath: String?
    private var quitting = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        VMController.installLogger()
        print("Apex Studio \(VMController.version) — \(VMController.hostCapabilities)")
        buildMenu()
        let args = CommandLine.arguments.dropFirst().filter { !$0.hasPrefix("-") }
        if let path = args.first {
            launch(profile: path)
        } else {
            chooseProfile()
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let c = controller, c.handle != nil else { return .terminateNow }
        // Stop the guest cleanly, terminate once the VMM reports back.
        quitting = true
        c.requestStop()
        return .terminateLater
    }

    private func chooseProfile() {
        let panel = NSOpenPanel()
        panel.title = "Choose an Apex device profile (.toml)"
        panel.allowedContentTypes = []
        panel.allowsOtherFileTypes = true
        panel.canChooseDirectories = false
        if panel.runModal() == .OK, let url = panel.url {
            launch(profile: url.path)
        } else {
            NSApp.terminate(nil)
        }
    }

    private func launch(profile path: String) {
        profilePath = path
        let controller = VMController(profilePath: path)
        self.controller = controller
        controller.onSerial = { data in
            FileHandle.standardOutput.write(data)
        }
        controller.onStopped = { [weak self] reason, message in
            self?.vmStopped(reason: reason, message: message)
        }
        do {
            try controller.boot()
        } catch {
            fail("Could not start the virtual phone", error.localizedDescription)
            return
        }
        let info = controller.displayInfo
        createWindow(width: Int(info.width), height: Int(info.height), refresh: Int(info.refresh_hz))
        battery = BatteryMirror(controller: controller)
        battery?.start()
    }

    private func createWindow(width: Int, height: Int, refresh: Int) {
        if let w = window {
            // Reboot: reuse the window.
            phoneView?.controller = controller
            renderer?.attach(vm: controller?.handle, hostVsync: false)
            w.makeKeyAndOrderFront(nil)
            return
        }
        let screen = NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let aspect = CGFloat(width) / CGFloat(max(height, 1))
        let h = min(screen.height * 0.9, 900)
        let w = h * aspect
        let panelWidth: CGFloat = 56

        let win = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: w + panelWidth, height: h),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        win.title = "Apex — \(URL(fileURLWithPath: profilePath ?? "").deletingPathExtension().lastPathComponent)"
        win.delegate = self
        win.backgroundColor = .black
        win.collectionBehavior = [.fullScreenPrimary]

        let phone = PhoneView(frame: NSRect(x: 0, y: 0, width: w, height: h))
        phone.configure(guestWidth: width, guestHeight: height)
        phone.controller = controller
        phoneView = phone

        let panel = HardwarePanel()
        panel.controller = controller
        panel.onToggleTrackpad = { [weak phone] on in phone?.trackpadMode = on }

        let hud = NSTextField(labelWithString: "")
        hud.font = .monospacedDigitSystemFont(ofSize: 11, weight: .medium)
        hud.textColor = .secondaryLabelColor
        self.hud = hud

        let side = NSStackView(views: [panel, hud])
        side.orientation = .vertical
        side.alignment = .centerX
        side.distribution = .fill
        side.setHuggingPriority(.required, for: .horizontal)

        let root = NSStackView(views: [phone, side])
        root.orientation = .horizontal
        root.spacing = 0
        root.distribution = .fill
        phone.setContentHuggingPriority(.defaultLow, for: .horizontal)
        side.widthAnchor.constraint(equalToConstant: panelWidth).isActive = true
        phone.widthAnchor.constraint(greaterThanOrEqualToConstant: 180).isActive = true
        phone.heightAnchor.constraint(greaterThanOrEqualToConstant: 320).isActive = true
        win.contentView = root
        win.contentAspectRatio = NSSize(width: w + panelWidth, height: h)
        win.center()
        win.makeKeyAndOrderFront(nil)
        win.makeFirstResponder(phone)
        window = win

        guard let r = Renderer(layer: phone.metalLayer) else {
            fail("Metal is unavailable", "Apex Studio needs a Metal capable GPU.")
            return
        }
        phone.renderer = r
        renderer = r
        r.attach(vm: controller?.handle, hostVsync: false)
        let hz = Float(max(refresh, 60))
        r.start(preferredHz: min(hz, Float(NSScreen.main?.maximumFramesPerSecond ?? 120)))

        hudTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.updateHud() }
    }

    private func updateHud() {
        guard let r = renderer, let c = controller, c.handle != nil else { return }
        let counters = r.counters()
        let stats = c.stats()
        let display = counters.presented - lastCounters.presented
        let fresh = counters.fresh - lastCounters.fresh
        let guest = stats.frames_submitted - lastCounters.submitted
        lastCounters = (counters.presented, counters.fresh, stats.frames_submitted)
        hud?.stringValue = "\(display) Hz\n\(guest) fps\n\(fresh) new"
    }

    private func vmStopped(reason: VMController.StopReason, message: String?) {
        battery?.stop()
        renderer?.attach(vm: nil, hostVsync: false)
        controller?.destroy()
        if quitting {
            NSApp.reply(toApplicationShouldTerminate: true)
            return
        }
        switch reason {
        case .reset:
            if let p = profilePath { launch(profile: p) }
        case .error:
            fail("The virtual phone stopped with an error", message ?? "unknown error")
        default:
            window?.close()
        }
    }

    private func fail(_ title: String, _ detail: String) {
        let a = NSAlert()
        a.messageText = title
        a.informativeText = detail
        a.alertStyle = .critical
        a.runModal()
    }

    func windowWillClose(_ notification: Notification) {
        renderer?.stop()
        hudTimer?.invalidate()
        controller?.requestStop()
    }

    private func buildMenu() {
        let main = NSMenu()
        let appItem = NSMenuItem()
        main.addItem(appItem)
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "About Apex Studio", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Apex Studio", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu

        let devItem = NSMenuItem()
        main.addItem(devItem)
        let dev = NSMenu(title: "Device")
        dev.addItem(withTitle: "Power", action: #selector(menuPower), keyEquivalent: "p")
        dev.addItem(withTitle: "Home", action: #selector(menuHome), keyEquivalent: "h").keyEquivalentModifierMask = [.command, .shift]
        dev.addItem(withTitle: "Back", action: #selector(menuBack), keyEquivalent: "\u{8}")
        dev.addItem(withTitle: "Recents", action: #selector(menuRecents), keyEquivalent: "r")
        dev.addItem(.separator())
        dev.addItem(withTitle: "Volume Up", action: #selector(menuVolUp), keyEquivalent: "=")
        dev.addItem(withTitle: "Volume Down", action: #selector(menuVolDown), keyEquivalent: "-")
        devItem.submenu = dev
        NSApp.mainMenu = main
    }

    @objc private func menuPower() { controller?.tap(key: UInt16(APEX_KEY_POWER)) }
    @objc private func menuHome() { controller?.tap(key: UInt16(APEX_KEY_HOMEPAGE)) }
    @objc private func menuBack() { controller?.tap(key: UInt16(APEX_KEY_BACK)) }
    @objc private func menuRecents() { controller?.tap(key: UInt16(APEX_KEY_APPSELECT)) }
    @objc private func menuVolUp() { controller?.tap(key: UInt16(APEX_KEY_VOLUMEUP)) }
    @objc private func menuVolDown() { controller?.tap(key: UInt16(APEX_KEY_VOLUMEDOWN)) }
}
