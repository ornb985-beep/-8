//! Display pipeline shared by virtio-gpu and the host frontend.
//!
//! ```text
//!  guest SurfaceFlinger ─RESOURCE_FLUSH─▶ virtio-gpu ──copy──▶ swapchain slot
//!                                                              │ (page aligned,
//!                                  vsync tick (120 Hz) ◀───────┘  UMA shared)
//!  guest fence signalled ◀── held flush responses released     ▼
//!                                             Metal: newBufferWithBytesNoCopy
//! ```
//!
//! * Three page-aligned slots: one being written, one latest, one held by
//!   the renderer. The frontend wraps each slot once with
//!   `newBufferWithBytesNoCopy`, so presenting costs zero copies on Apple
//!   Silicon's unified memory.
//! * The virtual vsync decides when the guest's page-flip fence signals,
//!   which is what paces SurfaceFlinger at exactly `refresh_hz`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use apex_core::sync::{StopFlag, Ticker};
use apex_core::{align_up, sys};

pub const SLOTS: usize = 3;
/// Row pitch alignment accepted by `MTLBuffer.makeTexture(bytesPerRow:)`.
pub const STRIDE_ALIGN: u32 = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub dpi: u32,
    pub name: String,
}

impl DisplayConfig {
    pub fn phone_default() -> DisplayConfig {
        DisplayConfig { width: 1080, height: 2400, refresh_hz: 120, dpi: 420, name: "Apex Display".into() }
    }

    /// Physical size in millimetres derived from the density.
    pub fn physical_mm(&self) -> (u32, u32) {
        let mm = |px: u32| ((px as f64 / self.dpi.max(1) as f64) * 25.4).round() as u32;
        (mm(self.width), mm(self.height))
    }

    pub fn frame_period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.refresh_hz.max(1) as u64)
    }
}

/// Byte order of a 32-bit pixel in memory.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    Bgra = 0,
    Rgba = 1,
    Argb = 2,
    Abgr = 3,
}

struct HostBuffer {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: plain memory owned by the swapchain.
unsafe impl Send for HostBuffer {}

impl HostBuffer {
    fn new(len: usize) -> Option<HostBuffer> {
        let page = sys::host_page_size();
        let len = align_up(len.max(page) as u64, page as u64) as usize;
        sys::mmap_anonymous(len).ok().map(|ptr| HostBuffer { ptr, len })
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated by mmap_anonymous with this length.
        let _ = unsafe { sys::munmap_raw(self.ptr, self.len) };
    }
}

struct Slot {
    buf: Option<HostBuffer>,
    generation: u64,
    width: u32,
    height: u32,
    stride: u32,
    layout: PixelLayout,
    opaque: bool,
    seq: u64,
}

impl Slot {
    fn empty() -> Slot {
        Slot { buf: None, generation: 0, width: 0, height: 0, stride: 0, layout: PixelLayout::Bgra, opaque: true, seq: 0 }
    }
}

struct Swap {
    slots: Vec<Slot>,
    latest: Option<usize>,
    writing: Option<usize>,
    held: [u32; SLOTS],
    seq: u64,
    next_generation: u64,
}

/// Frame handed to the frontend. `data` stays valid until `release(slot)`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub slot: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub layout: u32,
    pub opaque: u32,
    pub seq: u64,
    /// Changes whenever the slot's memory was reallocated (invalidate any
    /// cached MTLBuffer for this slot).
    pub generation: u64,
    pub data: *const u8,
    pub len: usize,
}

pub type VsyncListener = Box<dyn Fn(u64) + Send + Sync>;

#[derive(Default)]
pub struct DisplayStats {
    pub frames_submitted: AtomicU64,
    pub frames_presented: AtomicU64,
    pub vsyncs: AtomicU64,
    pub last_submit_ns: AtomicU64,
}

pub struct DisplayHub {
    cfg: RwLock<DisplayConfig>,
    swap: Mutex<Swap>,
    listeners: RwLock<Vec<VsyncListener>>,
    stats: DisplayStats,
    epoch: Instant,
    pacer: Mutex<Option<(Arc<StopFlag>, JoinHandle<()>)>>,
}

/// Exclusive access to a slot being filled by the GPU.
pub struct FrameWriter<'a> {
    hub: &'a DisplayHub,
    slot: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    ptr: *mut u8,
    committed: bool,
}

impl FrameWriter<'_> {
    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        assert!(y < self.height);
        // SAFETY: slot buffer is at least stride*height bytes and exclusively ours.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add((y * self.stride) as usize), (self.width * 4) as usize) }
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        (self.stride * self.height) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Publish as the latest frame.
    pub fn commit(mut self) {
        self.committed = true;
        let mut s = self.hub.swap.lock().unwrap();
        s.seq += 1;
        let seq = s.seq;
        s.slots[self.slot].seq = seq;
        s.latest = Some(self.slot);
        s.writing = None;
        self.hub.stats.frames_submitted.fetch_add(1, Ordering::Relaxed);
        self.hub.stats.last_submit_ns.store(self.hub.epoch.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

impl Drop for FrameWriter<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.hub.swap.lock().unwrap().writing = None;
        }
    }
}

impl DisplayHub {
    pub fn new(cfg: DisplayConfig) -> Arc<DisplayHub> {
        Arc::new(DisplayHub {
            cfg: RwLock::new(cfg),
            swap: Mutex::new(Swap {
                slots: (0..SLOTS).map(|_| Slot::empty()).collect(),
                latest: None,
                writing: None,
                held: [0; SLOTS],
                seq: 0,
                next_generation: 1,
            }),
            listeners: RwLock::new(Vec::new()),
            stats: DisplayStats::default(),
            epoch: Instant::now(),
            pacer: Mutex::new(None),
        })
    }

    pub fn config(&self) -> DisplayConfig {
        self.cfg.read().unwrap().clone()
    }

    pub fn stats(&self) -> &DisplayStats {
        &self.stats
    }

    /// Grab a slot to render a `width`x`height` frame into. Returns None if
    /// every slot is busy (frame dropped; the guest will flush again).
    pub fn begin_frame(&self, width: u32, height: u32, layout: PixelLayout, opaque: bool) -> Option<FrameWriter<'_>> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return None;
        }
        let stride = align_up(width as u64 * 4, STRIDE_ALIGN as u64) as u32;
        let need = stride as usize * height as usize;
        let mut s = self.swap.lock().unwrap();
        if s.writing.is_some() {
            return None;
        }
        let idx = (0..SLOTS).find(|&i| Some(i) != s.latest && s.held[i] == 0)?;
        let gen = s.next_generation;
        let slot = &mut s.slots[idx];
        let fits = slot.buf.as_ref().map(|b| b.len >= need).unwrap_or(false);
        let mut bumped = false;
        if !fits {
            slot.buf = Some(HostBuffer::new(need)?);
            slot.generation = gen;
            bumped = true;
        }
        slot.width = width;
        slot.height = height;
        slot.stride = stride;
        slot.layout = layout;
        slot.opaque = opaque;
        let ptr = slot.buf.as_ref().unwrap().ptr;
        if bumped {
            s.next_generation += 1;
        }
        s.writing = Some(idx);
        Some(FrameWriter { hub: self, slot: idx, width, height, stride, ptr, committed: false })
    }

    /// Latest frame newer than `after_seq`; the slot stays pinned until
    /// [`release`](Self::release).
    pub fn acquire(&self, after_seq: u64) -> Option<FrameInfo> {
        let mut s = self.swap.lock().unwrap();
        let idx = s.latest?;
        if s.slots[idx].seq <= after_seq {
            return None;
        }
        s.held[idx] += 1;
        let sl = &s.slots[idx];
        let buf = sl.buf.as_ref()?;
        self.stats.frames_presented.fetch_add(1, Ordering::Relaxed);
        Some(FrameInfo {
            slot: idx as u32,
            width: sl.width,
            height: sl.height,
            stride: sl.stride,
            layout: sl.layout as u32,
            opaque: sl.opaque as u32,
            seq: sl.seq,
            generation: sl.generation,
            data: buf.ptr,
            len: buf.len,
        })
    }

    pub fn release(&self, slot: u32) {
        let mut s = self.swap.lock().unwrap();
        if let Some(h) = s.held.get_mut(slot as usize) {
            *h = h.saturating_sub(1);
        }
    }

    pub fn add_vsync_listener(&self, l: VsyncListener) {
        self.listeners.write().unwrap().push(l);
    }

    /// One display refresh happened.
    pub fn vsync(&self) {
        let n = self.stats.vsyncs.fetch_add(1, Ordering::AcqRel) + 1;
        for l in self.listeners.read().unwrap().iter() {
            l(n);
        }
    }

    /// Drive vsync from an internal high-precision timer at `refresh_hz`.
    pub fn start_internal_vsync(self: &Arc<Self>) {
        let mut p = self.pacer.lock().unwrap();
        if p.is_some() {
            return;
        }
        let stop = Arc::new(StopFlag::new());
        let (hub, s) = (Arc::downgrade(self), stop.clone());
        let period = self.config().frame_period();
        let t = std::thread::Builder::new()
            .name("apex-vsync".into())
            .spawn(move || {
                sys::set_thread_latency_critical();
                let mut ticker = Ticker::new(period);
                while !s.is_stopped() {
                    ticker.wait();
                    match hub.upgrade() {
                        Some(h) => h.vsync(),
                        None => break,
                    }
                }
            })
            .expect("spawn vsync thread");
        *p = Some((stop, t));
    }

    pub fn stop_internal_vsync(&self) {
        if let Some((s, t)) = self.pacer.lock().unwrap().take() {
            s.stop();
            let _ = t.join();
        }
    }
}

impl Drop for DisplayHub {
    fn drop(&mut self) {
        if let Some((s, _)) = self.pacer.get_mut().unwrap().take() {
            s.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_buffering_never_blocks_writer() {
        let hub = DisplayHub::new(DisplayConfig::phone_default());
        let mut last = 0;
        for i in 0..10u8 {
            let mut w = hub.begin_frame(64, 32, PixelLayout::Bgra, true).unwrap();
            assert_eq!(w.stride % STRIDE_ALIGN, 0);
            w.row_mut(0)[0] = i;
            w.commit();
            // Frontend holds the frame it acquired while the GPU keeps going.
            let f = hub.acquire(last).unwrap();
            assert!(f.seq > last);
            assert_eq!(unsafe { *f.data }, i);
            assert_eq!(f.data as usize % sys::host_page_size(), 0);
            assert_eq!(f.len % sys::host_page_size(), 0);
            last = f.seq;
            let w2 = hub.begin_frame(64, 32, PixelLayout::Bgra, true).unwrap();
            drop(w2); // aborted frame
            hub.release(f.slot);
        }
        assert!(hub.acquire(last).is_none(), "no new frame");
    }

    #[test]
    fn resize_bumps_generation() {
        let hub = DisplayHub::new(DisplayConfig::phone_default());
        hub.begin_frame(16, 16, PixelLayout::Rgba, false).unwrap().commit();
        let a = hub.acquire(0).unwrap();
        hub.release(a.slot);
        let mut gens = vec![a.generation];
        for _ in 0..3 {
            hub.begin_frame(2000, 2000, PixelLayout::Rgba, false).unwrap().commit();
            let f = hub.acquire(0).unwrap();
            gens.push(f.generation);
            hub.release(f.slot);
        }
        assert!(gens.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn internal_vsync_runs_at_rate() {
        let mut cfg = DisplayConfig::phone_default();
        cfg.refresh_hz = 240;
        let hub = DisplayHub::new(cfg);
        let count = Arc::new(AtomicU64::new(0));
        let c = count.clone();
        hub.add_vsync_listener(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        hub.start_internal_vsync();
        std::thread::sleep(Duration::from_millis(210));
        hub.stop_internal_vsync();
        let n = count.load(Ordering::SeqCst);
        assert!((35..=60).contains(&n), "vsyncs in 210ms at 240Hz: {n}");
    }

    #[test]
    fn physical_size() {
        let c = DisplayConfig::phone_default();
        assert_eq!(c.physical_mm(), (65, 145));
        assert_eq!(c.frame_period(), Duration::from_nanos(8_333_333));
    }
}
