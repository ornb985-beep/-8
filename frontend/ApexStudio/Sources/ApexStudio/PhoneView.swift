import AppKit
import CApex
import QuartzCore

/// The phone screen: a CAMetalLayer-backed view that turns mouse, trackpad
/// and keyboard input into touchscreen / key events for the guest.
///
/// * Click + drag           → one finger
/// * ⌥ Option + drag        → two-finger pinch/rotate around the screen centre
/// * Right click            → Back
/// * Trackpad mode (toolbar) → every finger on the Mac trackpad becomes a
///   finger on the phone (absolute mapping, up to 10 contacts)
final class PhoneView: NSView {
    var controller: VMController?
    var renderer: Renderer?
    var trackpadMode = false {
        didSet {
            allowedTouchTypes = trackpadMode ? [.indirect] : []
            wantsRestingTouches = false
        }
    }

    private var guestSize = CGSize(width: 1080, height: 2400)
    private var pinchActive = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layerContentsRedrawPolicy = .never
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func makeBackingLayer() -> CALayer {
        let l = CAMetalLayer()
        l.contentsScale = NSScreen.main?.backingScaleFactor ?? 2
        return l
    }

    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    func configure(guestWidth: Int, guestHeight: Int) {
        guestSize = CGSize(width: guestWidth, height: guestHeight)
    }

    override var acceptsFirstResponder: Bool { true }
    override var isOpaque: Bool { true }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        metalLayer.contentsScale = window?.backingScaleFactor ?? 2
        updateDrawableSize()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        updateDrawableSize()
    }

    private func updateDrawableSize() {
        let scale = window?.backingScaleFactor ?? metalLayer.contentsScale
        metalLayer.drawableSize = CGSize(width: bounds.width * scale, height: bounds.height * scale)
    }

    // MARK: coordinate mapping

    /// Letterboxed guest screen rectangle in view points (origin bottom-left).
    private var screenRect: CGRect {
        let scale = min(bounds.width / guestSize.width, bounds.height / guestSize.height)
        let w = guestSize.width * scale, h = guestSize.height * scale
        return CGRect(x: (bounds.width - w) / 2, y: (bounds.height - h) / 2, width: w, height: h)
    }

    private func guestPoint(_ p: CGPoint) -> (Int32, Int32) {
        let r = screenRect
        let nx = (p.x - r.minX) / r.width
        let ny = 1 - (p.y - r.minY) / r.height
        let x = Int32((nx * guestSize.width).rounded(.down))
        let y = Int32((ny * guestSize.height).rounded(.down))
        return (max(0, min(x, Int32(guestSize.width) - 1)), max(0, min(y, Int32(guestSize.height) - 1)))
    }

    private func contact(_ id: UInt32, _ p: CGPoint, pressure: Int32 = 160) -> ApexTouch {
        let (x, y) = guestPoint(p)
        return ApexTouch(id: id, x: x, y: y, pressure: pressure, major: 8)
    }

    // MARK: mouse → touch

    private func sendMouseTouch(_ event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        if event.modifierFlags.contains(.option) {
            // Mirror the pointer around the centre of the screen: pinch.
            let c = CGPoint(x: screenRect.midX, y: screenRect.midY)
            let mirrored = CGPoint(x: 2 * c.x - p.x, y: 2 * c.y - p.y)
            pinchActive = true
            controller?.touch([contact(1, p), contact(2, mirrored)])
        } else {
            pinchActive = false
            controller?.touch([contact(1, p)])
        }
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        guard !trackpadMode else { return }
        sendMouseTouch(event)
    }

    override func mouseDragged(with event: NSEvent) {
        guard !trackpadMode else { return }
        sendMouseTouch(event)
    }

    override func mouseUp(with event: NSEvent) {
        guard !trackpadMode else { return }
        pinchActive = false
        controller?.touch([])
    }

    override func rightMouseDown(with event: NSEvent) {
        controller?.key(UInt16(APEX_KEY_BACK), down: true)
    }

    override func rightMouseUp(with event: NSEvent) {
        controller?.key(UInt16(APEX_KEY_BACK), down: false)
    }

    // MARK: trackpad → multitouch

    private func sendTrackpad(_ event: NSEvent) {
        let touches = event.touches(matching: .touching, in: self)
        let r = screenRect
        var contacts: [ApexTouch] = []
        for t in touches {
            let n = t.normalizedPosition
            let p = CGPoint(x: r.minX + n.x * r.width, y: r.minY + n.y * r.height)
            let id = UInt32(truncatingIfNeeded: (t.identity as AnyObject).hash)
            contacts.append(contact(id, p))
        }
        controller?.touch(contacts)
    }

    override func touchesBegan(with event: NSEvent) { if trackpadMode { sendTrackpad(event) } }
    override func touchesMoved(with event: NSEvent) { if trackpadMode { sendTrackpad(event) } }
    override func touchesEnded(with event: NSEvent) { if trackpadMode { sendTrackpad(event) } }
    override func touchesCancelled(with event: NSEvent) { if trackpadMode { controller?.touch([]) } }

    // MARK: keyboard

    override func keyDown(with event: NSEvent) {
        if event.modifierFlags.contains(.command) {
            super.keyDown(with: event)
            return
        }
        let code = apex_mac_keycode_to_linux(event.keyCode)
        if code != 0 { controller?.key(code, down: true) }
    }

    override func keyUp(with event: NSEvent) {
        let code = apex_mac_keycode_to_linux(event.keyCode)
        if code != 0 { controller?.key(code, down: false) }
    }

    override func flagsChanged(with event: NSEvent) {
        let code = apex_mac_keycode_to_linux(event.keyCode)
        guard code != 0 else { return }
        let flag: NSEvent.ModifierFlags
        switch event.keyCode {
        case 0x38, 0x3c: flag = .shift
        case 0x3b, 0x3e: flag = .control
        case 0x3a, 0x3d: flag = .option
        case 0x39: flag = .capsLock
        default: return
        }
        controller?.key(code, down: event.modifierFlags.contains(flag))
    }
}
