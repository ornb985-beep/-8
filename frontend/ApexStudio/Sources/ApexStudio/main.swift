import AppKit

// Apex Studio: native macOS shell around the Apex-AOSP VMM.
// Usage: ApexStudio [profile.toml]
let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
