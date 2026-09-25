import AppKit
import CApex

/// Side panel with the phone's hardware buttons and host integrations.
final class HardwarePanel: NSStackView {
    var controller: VMController?
    var onToggleTrackpad: ((Bool) -> Void)?
    private var trackpadButton: NSButton?

    init() {
        super.init(frame: .zero)
        orientation = .vertical
        alignment = .centerX
        spacing = 10
        edgeInsets = NSEdgeInsets(top: 16, left: 8, bottom: 16, right: 8)

        addArrangedSubview(button("power", "Power", key: UInt16(APEX_KEY_POWER)))
        addArrangedSubview(button("speaker.plus", "Volume up", key: UInt16(APEX_KEY_VOLUMEUP)))
        addArrangedSubview(button("speaker.minus", "Volume down", key: UInt16(APEX_KEY_VOLUMEDOWN)))
        addArrangedSubview(separator())
        addArrangedSubview(button("arrow.uturn.backward", "Back", key: UInt16(APEX_KEY_BACK)))
        addArrangedSubview(button("circle", "Home", key: UInt16(APEX_KEY_HOMEPAGE)))
        addArrangedSubview(button("square.on.square", "Recents", key: UInt16(APEX_KEY_APPSELECT)))
        addArrangedSubview(separator())

        let tp = NSButton(image: symbol("hand.point.up.left", "Trackpad multitouch"), target: self, action: #selector(toggleTrackpad(_:)))
        tp.setButtonType(.pushOnPushOff)
        tp.bezelStyle = .regularSquare
        tp.toolTip = "Map every finger on the Mac trackpad to the phone screen"
        trackpadButton = tp
        addArrangedSubview(tp)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    private func symbol(_ name: String, _ label: String) -> NSImage {
        NSImage(systemSymbolName: name, accessibilityDescription: label) ?? NSImage()
    }

    private func separator() -> NSView {
        let b = NSBox()
        b.boxType = .separator
        b.widthAnchor.constraint(equalToConstant: 28).isActive = true
        return b
    }

    private func button(_ symbolName: String, _ label: String, key: UInt16) -> NSButton {
        let b = NSButton(image: symbol(symbolName, label), target: self, action: #selector(pressed(_:)))
        b.bezelStyle = .regularSquare
        b.tag = Int(key)
        b.toolTip = label
        b.widthAnchor.constraint(equalToConstant: 36).isActive = true
        b.heightAnchor.constraint(equalToConstant: 32).isActive = true
        return b
    }

    @objc private func pressed(_ sender: NSButton) {
        // A long press on Power opens the power menu on Android.
        let hold = NSEvent.modifierFlags.contains(.option) ? 1200 : 60
        controller?.tap(key: UInt16(sender.tag), holdMs: hold)
    }

    @objc private func toggleTrackpad(_ sender: NSButton) {
        onToggleTrackpad?(sender.state == .on)
    }
}
