import CApex
import Metal
import QuartzCore

/// Keeps a guest frame pinned in the VMM swapchain for as long as any GPU
/// command buffer may still read it.
private final class PinnedFrame {
    let frame: ApexFrame
    let texture: MTLTexture
    private let vm: OpaquePointer

    init(frame: ApexFrame, texture: MTLTexture, vm: OpaquePointer) {
        self.frame = frame
        self.texture = texture
        self.vm = vm
    }

    deinit {
        apex_display_release(vm, frame.slot)
    }
}

/// Presents guest frames at the panel's native rate (120 Hz on ProMotion)
/// using CAMetalDisplayLink on a dedicated render thread, so UI work on the
/// main thread never delays a vsync.
///
/// Zero copy: each swapchain slot is wrapped once in an MTLBuffer created
/// with `bytesNoCopy`; the GPU samples guest pixels straight out of the
/// memory the virtio-gpu device wrote (unified memory on Apple Silicon).
final class Renderer: NSObject, CAMetalDisplayLinkDelegate {
    let device: MTLDevice
    let layer: CAMetalLayer
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private var link: CAMetalDisplayLink?
    private var thread: Thread?

    private let lock = NSLock()
    private var vm: OpaquePointer?
    private var hostVsync = false
    private var lastSeq: UInt64 = 0
    private var current: PinnedFrame?
    private var buffers: [UInt32: (generation: UInt64, buffer: MTLBuffer)] = [:]
    /// Letterboxed rectangle of the guest screen inside the layer, in
    /// drawable pixels (used for input mapping too).
    private(set) var contentRect = CGRect.zero

    // Statistics, updated on the render thread.
    private var presentedFrames: UInt64 = 0
    private var newFrames: UInt64 = 0

    /// (display refreshes rendered, distinct guest frames shown) — thread safe.
    func counters() -> (presented: UInt64, fresh: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        return (presentedFrames, newFrames)
    }

    init?(layer: CAMetalLayer) {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else { return nil }
        self.device = device
        self.layer = layer
        self.queue = queue
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.colorspace = CGColorSpace(name: CGColorSpace.sRGB)
        layer.maximumDrawableCount = 3
        layer.displaySyncEnabled = true
        do {
            let lib = try device.makeLibrary(source: Shaders.source, options: nil)
            let desc = MTLRenderPipelineDescriptor()
            desc.vertexFunction = lib.makeFunction(name: "apex_vertex")
            desc.fragmentFunction = lib.makeFunction(name: "apex_fragment")
            desc.colorAttachments[0].pixelFormat = layer.pixelFormat
            pipeline = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            print("ApexStudio: Metal pipeline failed: \(error)")
            return nil
        }
        super.init()
    }

    func attach(vm: OpaquePointer?, hostVsync: Bool) {
        lock.lock()
        self.vm = vm
        self.hostVsync = hostVsync
        current = nil
        buffers.removeAll()
        lastSeq = 0
        lock.unlock()
    }

    func start(preferredHz: Float) {
        guard thread == nil else { return }
        let t = Thread { [weak self] in
            guard let self else { return }
            let link = CAMetalDisplayLink(metalLayer: self.layer)
            link.delegate = self
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: preferredHz, preferred: preferredHz)
            link.preferredFrameLatency = 1
            link.add(to: .current, forMode: .default)
            self.link = link
            while !Thread.current.isCancelled {
                RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.25))
            }
            link.invalidate()
        }
        t.name = "apex-render"
        t.qualityOfService = .userInteractive
        thread = t
        t.start()
    }

    func stop() {
        thread?.cancel()
        thread = nil
    }

    private func buffer(for f: ApexFrame) -> MTLBuffer? {
        if let cached = buffers[f.slot], cached.generation == f.generation {
            return cached.buffer
        }
        guard let base = f.data else { return nil }
        let ptr = UnsafeMutableRawPointer(mutating: base)
        guard let b = device.makeBuffer(bytesNoCopy: ptr, length: f.len, options: .storageModeShared, deallocator: nil) else {
            return nil
        }
        buffers[f.slot] = (f.generation, b)
        return b
    }

    /// Pick up the newest guest frame, if any.
    private func refreshFrame(vm: OpaquePointer) {
        var f = ApexFrame()
        guard apex_display_acquire(vm, lastSeq, &f) else { return }
        lastSeq = f.seq
        guard let buf = buffer(for: f) else {
            apex_display_release(vm, f.slot)
            return
        }
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: Int(f.width), height: Int(f.height), mipmapped: false)
        desc.storageMode = .shared
        desc.usage = .shaderRead
        guard let tex = buf.makeTexture(descriptor: desc, offset: 0, bytesPerRow: Int(f.stride)) else {
            apex_display_release(vm, f.slot)
            return
        }
        // Dropping the previous PinnedFrame releases its slot once the last
        // command buffer that sampled it has completed.
        current = PinnedFrame(frame: f, texture: tex, vm: vm)
        newFrames += 1
    }

    func metalDisplayLink(_ link: CAMetalDisplayLink, needsUpdate update: CAMetalDisplayLink.Update) {
        lock.lock()
        defer { lock.unlock() }
        guard let vm else { return }
        if hostVsync {
            apex_display_vsync(vm)
        }
        refreshFrame(vm: vm)

        let drawable = update.drawable
        guard let cmd = queue.makeCommandBuffer() else { return }
        let rp = MTLRenderPassDescriptor()
        rp.colorAttachments[0].texture = drawable.texture
        rp.colorAttachments[0].loadAction = .clear
        rp.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        rp.colorAttachments[0].storeAction = .store
        guard let enc = cmd.makeRenderCommandEncoder(descriptor: rp) else { return }

        if let frame = current {
            let dw = Double(drawable.texture.width), dh = Double(drawable.texture.height)
            let fw = Double(frame.frame.width), fh = Double(frame.frame.height)
            let scale = min(dw / fw, dh / fh)
            let w = fw * scale, h = fh * scale
            let rect = CGRect(x: (dw - w) / 2, y: (dh - h) / 2, width: w, height: h)
            contentRect = rect
            enc.setViewport(MTLViewport(originX: rect.minX, originY: rect.minY, width: rect.width, height: rect.height, znear: 0, zfar: 1))
            enc.setRenderPipelineState(pipeline)
            enc.setFragmentTexture(frame.texture, index: 0)
            var uniforms: (UInt32, UInt32) = (frame.frame.layout, frame.frame.opaque)
            enc.setFragmentBytes(&uniforms, length: MemoryLayout<(UInt32, UInt32)>.size, index: 0)
            enc.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
            cmd.addCompletedHandler { _ in
                withExtendedLifetime(frame) {}
            }
        }
        enc.endEncoding()
        cmd.present(drawable)
        cmd.commit()
        presentedFrames += 1
    }
}
