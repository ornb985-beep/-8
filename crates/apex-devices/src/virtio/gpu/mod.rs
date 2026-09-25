//! virtio-gpu.
//!
//! * 2D resources and guest-memory blobs (dumb buffers used by
//!   drm_hwcomposer / minigbm) are composed on the CPU straight into the
//!   display swapchain.
//! * Page flips are paced: the fence of a fenced RESOURCE_FLUSH is only
//!   returned to the guest at the next virtual vsync, so SurfaceFlinger's
//!   frame clock locks onto `refresh_hz` (120 Hz by default).
//! * With a 3D renderer (gfxstream) the CONTEXT_INIT/blob path is used:
//!   guest GLES/Vulkan command streams go to the host GPU, host-visible
//!   blobs are mapped into the guest through the shared memory window
//!   (zero copy on unified memory), and per-context fence timelines are
//!   honoured.

pub mod edid;
pub mod gfxstream;
pub mod protocol;
pub mod renderer;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use apex_core::mem::{ByteValued, GuestAddress, GuestMemory};
use apex_core::sync::{Event, StopFlag};
use apex_core::{sys, Error, Result};

use super::{device_type, ActivateContext, DescriptorChain, Queue, Reader, ShmRegion, VirtioDevice, VirtioInterrupt, Writer};
use crate::display::DisplayHub;
use protocol::*;
use renderer::{FenceId, HostMapper, Iov, Renderer3d};

const CTRLQ: usize = 0;
const CURSORQ: usize = 1;
const MAX_2D_BYTES: u64 = 1 << 30;
const MAX_BACKING_ENTRIES: u32 = 1 << 16;

/// Factory invoked on the GPU worker thread when the driver activates the
/// device (renderers may be thread-affine).
pub type RendererFactory = Box<dyn Fn(renderer::FenceSink) -> Result<Box<dyn Renderer3d>> + Send + Sync>;

pub struct GpuOptions {
    pub display: Arc<DisplayHub>,
    pub renderer: Option<RendererFactory>,
    /// Guest physical window for host-visible blobs + the mapper to use.
    pub hostmem: Option<(u64, u64, Arc<dyn HostMapper>)>,
    /// Hold fenced flushes until vsync (true in production).
    pub pace_flushes: bool,
    pub edid_serial: u32,
}

enum ResKind {
    /// Classic 2D resource: host copy updated by TRANSFER_TO_HOST_2D.
    Classic { data: Vec<u8> },
    /// Guest memory blob: the backing *is* the content.
    GuestBlob { size: u64 },
    /// Owned by the 3D renderer.
    Renderer { size: u64 },
}

struct Resource {
    width: u32,
    height: u32,
    format: u32,
    kind: ResKind,
    backing: Vec<Iov>,
    mapped: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Default)]
struct Scanout {
    resource_id: u32,
    rect: Rect,
    blob: Option<SetScanoutBlob>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Timeline {
    Global,
    Ring(u32, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    None,
    Vsync,
    Renderer,
}

struct Held {
    head: u16,
    len: u32,
    fence_id: u64,
    wait: Wait,
}

enum Completion {
    Now(u32),
    Fenced { len: u32, timeline: Timeline, fence_id: u64, wait: Wait },
}

struct Shared {
    kick: Event,
    stop: StopFlag,
    vsync_seq: AtomicU64,
    signalled: Mutex<Vec<FenceId>>,
}

struct Worker {
    mem: GuestMemory,
    queues: Vec<Queue>,
    irq: VirtioInterrupt,
    display: Arc<DisplayHub>,
    renderer: Option<Box<dyn Renderer3d>>,
    hostmem: Option<(u64, u64, Arc<dyn HostMapper>)>,
    pace: bool,
    edid: [u8; 128],
    resources: HashMap<u32, Resource>,
    scanout: Scanout,
    timelines: HashMap<Timeline, VecDeque<Held>>,
    seen_vsync: u64,
    bytes_2d: u64,
    shared: Arc<Shared>,
    frames: u64,
}

fn iovs_for(mem: &GuestMemory, entries: &[MemEntry]) -> std::result::Result<Vec<Iov>, u32> {
    let mut v = Vec::with_capacity(entries.len());
    for e in entries {
        let p = mem.host_ptr(GuestAddress(e.addr), e.length as u64).map_err(|_| resp::ERR_INVALID_PARAMETER)?;
        v.push(Iov { base: p, len: e.length as usize });
    }
    Ok(v)
}

/// Copy `dst.len()` bytes starting at linear offset `off` of a scatter list.
fn sg_copy(iovs: &[Iov], mut off: usize, dst: &mut [u8]) -> bool {
    let mut done = 0;
    for iov in iovs {
        if off >= iov.len {
            off -= iov.len;
            continue;
        }
        let n = (iov.len - off).min(dst.len() - done);
        // SAFETY: iovs point into validated guest memory.
        unsafe { std::ptr::copy_nonoverlapping(iov.base.add(off), dst[done..].as_mut_ptr(), n) };
        done += n;
        off = 0;
        if done == dst.len() {
            return true;
        }
    }
    done == dst.len()
}

fn sg_len(iovs: &[Iov]) -> u64 {
    iovs.iter().map(|i| i.len as u64).sum()
}

impl Worker {
    fn reply(w: &mut Writer, hdr: &CtrlHdr, type_: u32) -> u32 {
        let h = CtrlHdr::response(hdr, type_);
        w.write(h.as_bytes()) as u32
    }

    fn reply_with<T: ByteValued>(w: &mut Writer, hdr: &CtrlHdr, type_: u32, body: &T) -> u32 {
        Self::reply(w, hdr, type_) + w.write(body.as_bytes()) as u32
    }

    fn handle(&mut self, chain: &DescriptorChain) -> Completion {
        let mem = self.mem.clone();
        let mut r = chain.reader(&mem);
        let mut w = chain.writer(&mem);
        let hdr: CtrlHdr = match r.read_obj() {
            Ok(h) => h,
            Err(_) => return Completion::Now(0),
        };
        let (len, wait) = match self.dispatch(&hdr, &mut r, &mut w) {
            Ok((len, wait)) => (len, wait),
            Err(code) => (Self::reply(&mut w, &hdr, code), Wait::None),
        };
        if hdr.flags & FLAG_FENCE == 0 {
            return Completion::Now(len);
        }
        let timeline = if hdr.flags & FLAG_INFO_RING_IDX != 0 { Timeline::Ring(hdr.ctx_id, hdr.ring_idx) } else { Timeline::Global };
        // Commands executed by the renderer complete when it says so.
        let wait = if wait == Wait::Renderer {
            let fid = FenceId {
                ctx_id: hdr.ctx_id,
                ring_idx: (hdr.flags & FLAG_INFO_RING_IDX != 0).then_some(hdr.ring_idx),
                fence_id: hdr.fence_id,
            };
            match self.renderer.as_mut().map(|rd| rd.create_fence(fid)) {
                Some(Ok(())) => Wait::Renderer,
                _ => Wait::None,
            }
        } else if wait == Wait::Vsync && !self.pace {
            Wait::None
        } else {
            wait
        };
        Completion::Fenced { len, timeline, fence_id: hdr.fence_id, wait }
    }

    fn dispatch(&mut self, hdr: &CtrlHdr, r: &mut Reader, w: &mut Writer) -> std::result::Result<(u32, Wait), u32> {
        let ok = |w: &mut Writer| Ok((Self::reply(w, hdr, resp::OK_NODATA), Wait::None));
        let cfg = self.display.config();
        match hdr.type_ {
            cmd::GET_DISPLAY_INFO => {
                let mut pm = [DisplayOne::default(); MAX_SCANOUTS];
                pm[0] = DisplayOne { r: Rect { x: 0, y: 0, width: cfg.width, height: cfg.height }, enabled: 1, flags: 0 };
                let mut n = Self::reply(w, hdr, resp::OK_DISPLAY_INFO);
                for p in pm {
                    n += w.write(p.as_bytes()) as u32;
                }
                Ok((n, Wait::None))
            }
            cmd::GET_EDID => {
                let q: GetEdid = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if q.scanout != 0 {
                    return Err(resp::ERR_INVALID_SCANOUT_ID);
                }
                let mut n = Self::reply(w, hdr, resp::OK_EDID);
                n += w.write(&128u32.to_le_bytes()) as u32;
                n += w.write(&0u32.to_le_bytes()) as u32;
                let mut blob = [0u8; 1024];
                blob[..128].copy_from_slice(&self.edid);
                n += w.write(&blob) as u32;
                Ok((n, Wait::None))
            }
            cmd::RESOURCE_CREATE_2D => {
                let c: ResourceCreate2d = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if c.resource_id == 0 || self.resources.contains_key(&c.resource_id) {
                    return Err(resp::ERR_INVALID_RESOURCE_ID);
                }
                format_layout(c.format).ok_or(resp::ERR_INVALID_PARAMETER)?;
                if c.width == 0 || c.height == 0 || c.width > 16384 || c.height > 16384 {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                let bytes = c.width as u64 * c.height as u64 * 4;
                if self.bytes_2d + bytes > MAX_2D_BYTES {
                    return Err(resp::ERR_OUT_OF_MEMORY);
                }
                self.bytes_2d += bytes;
                self.resources.insert(
                    c.resource_id,
                    Resource {
                        width: c.width,
                        height: c.height,
                        format: c.format,
                        kind: ResKind::Classic { data: vec![0u8; bytes as usize] },
                        backing: Vec::new(),
                        mapped: None,
                    },
                );
                ok(w)
            }
            cmd::RESOURCE_UNREF => {
                let c: ResourceId = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let res = self.resources.remove(&c.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if let ResKind::Classic { data } = &res.kind {
                    self.bytes_2d -= data.len() as u64;
                }
                if let Some((off, size)) = res.mapped {
                    self.unmap_hostmem(off, size);
                }
                if matches!(res.kind, ResKind::Renderer { .. }) {
                    if let Some(rd) = self.renderer.as_mut() {
                        rd.resource_unref(c.resource_id);
                    }
                }
                if self.scanout.resource_id == c.resource_id {
                    self.scanout = Scanout::default();
                }
                ok(w)
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let a: AttachBacking = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if a.nr_entries > MAX_BACKING_ENTRIES {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                let mut entries = Vec::with_capacity(a.nr_entries as usize);
                for _ in 0..a.nr_entries {
                    entries.push(r.read_obj::<MemEntry>().map_err(|_| resp::ERR_INVALID_PARAMETER)?);
                }
                let iovs = iovs_for(&self.mem, &entries)?;
                let res = self.resources.get_mut(&a.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if matches!(res.kind, ResKind::Renderer { .. }) {
                    self.renderer.as_mut().ok_or(resp::ERR_UNSPEC)?.attach_backing(a.resource_id, &iovs)?;
                }
                res.backing = iovs;
                ok(w)
            }
            cmd::RESOURCE_DETACH_BACKING => {
                let c: ResourceId = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let res = self.resources.get_mut(&c.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if matches!(res.kind, ResKind::Renderer { .. }) {
                    if let Some(rd) = self.renderer.as_mut() {
                        rd.detach_backing(c.resource_id);
                    }
                }
                res.backing.clear();
                ok(w)
            }
            cmd::SET_SCANOUT => {
                let s: SetScanout = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if s.scanout_id != 0 {
                    return Err(resp::ERR_INVALID_SCANOUT_ID);
                }
                if s.resource_id == 0 || s.r.is_empty() {
                    self.scanout = Scanout::default();
                    return ok(w);
                }
                let res = self.resources.get(&s.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if !s.r.within(res.width, res.height) {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                self.scanout = Scanout { resource_id: s.resource_id, rect: s.r, blob: None };
                ok(w)
            }
            cmd::SET_SCANOUT_BLOB => {
                let s: SetScanoutBlob = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if s.scanout_id != 0 {
                    return Err(resp::ERR_INVALID_SCANOUT_ID);
                }
                if s.resource_id == 0 {
                    self.scanout = Scanout::default();
                    return ok(w);
                }
                let res = self.resources.get(&s.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                format_layout(s.format).ok_or(resp::ERR_INVALID_PARAMETER)?;
                let size = match res.kind {
                    ResKind::GuestBlob { size } | ResKind::Renderer { size } => size,
                    ResKind::Classic { .. } => return Err(resp::ERR_INVALID_PARAMETER),
                };
                let need = s.offsets[0] as u64 + s.strides[0] as u64 * (s.height.max(1) as u64 - 1) + s.width as u64 * 4;
                if s.width == 0
                    || s.height == 0
                    || (s.strides[0] as u64) < s.width as u64 * 4
                    || need > size
                    || !s.r.within(s.width, s.height)
                {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                self.scanout = Scanout { resource_id: s.resource_id, rect: s.r, blob: Some(s) };
                ok(w)
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let t: TransferToHost2d = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let res = self.resources.get_mut(&t.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if !t.r.within(res.width, res.height) {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                let stride = res.width as usize * 4;
                match &mut res.kind {
                    ResKind::Classic { data } => {
                        if res.backing.is_empty() {
                            return Err(resp::ERR_UNSPEC);
                        }
                        let row = t.r.width as usize * 4;
                        for h in 0..t.r.height as usize {
                            let src = t.offset as usize + h * stride;
                            let dst = (t.r.y as usize + h) * stride + t.r.x as usize * 4;
                            if !sg_copy(&res.backing, src, &mut data[dst..dst + row]) {
                                return Err(resp::ERR_INVALID_PARAMETER);
                            }
                        }
                        ok(w)
                    }
                    ResKind::Renderer { .. } => {
                        let t3 = Transfer3d {
                            box_: Box3d { x: t.r.x, y: t.r.y, z: 0, w: t.r.width, h: t.r.height, d: 1 },
                            offset: t.offset,
                            resource_id: t.resource_id,
                            level: 0,
                            stride: 0,
                            layer_stride: 0,
                        };
                        self.renderer.as_mut().ok_or(resp::ERR_UNSPEC)?.transfer_write(0, &t3)?;
                        ok(w)
                    }
                    ResKind::GuestBlob { .. } => ok(w), // shared memory: nothing to copy
                }
            }
            cmd::RESOURCE_FLUSH => {
                let f: ResourceFlush = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if !self.resources.contains_key(&f.resource_id) {
                    return Err(resp::ERR_INVALID_RESOURCE_ID);
                }
                if self.scanout.resource_id == f.resource_id {
                    self.present();
                    return Ok((Self::reply(w, hdr, resp::OK_NODATA), Wait::Vsync));
                }
                ok(w)
            }
            cmd::GET_CAPSET_INFO => {
                let q: GetCapsetInfo = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let caps = self.renderer.as_ref().map(|rd| rd.capsets()).unwrap_or_default();
                let (id, ver, size) = caps.get(q.capset_index as usize).copied().unwrap_or((0, 0, 0));
                let body = RespCapsetInfo { capset_id: id, capset_max_version: ver, capset_max_size: size, padding: 0 };
                Ok((Self::reply_with(w, hdr, resp::OK_CAPSET_INFO, &body), Wait::None))
            }
            cmd::GET_CAPSET => {
                let q: GetCapset = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let rd = self.renderer.as_ref().ok_or(resp::ERR_INVALID_PARAMETER)?;
                let data = rd.fill_caps(q.capset_id, q.capset_version);
                let n = Self::reply(w, hdr, resp::OK_CAPSET) + w.write(&data) as u32;
                Ok((n, Wait::None))
            }
            cmd::RESOURCE_CREATE_BLOB => {
                let b: ResourceCreateBlob = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if b.resource_id == 0 || self.resources.contains_key(&b.resource_id) || b.nr_entries > MAX_BACKING_ENTRIES {
                    return Err(resp::ERR_INVALID_RESOURCE_ID);
                }
                let mut entries = Vec::with_capacity(b.nr_entries as usize);
                for _ in 0..b.nr_entries {
                    entries.push(r.read_obj::<MemEntry>().map_err(|_| resp::ERR_INVALID_PARAMETER)?);
                }
                let iovs = iovs_for(&self.mem, &entries)?;
                let kind = match b.blob_mem {
                    blob_mem::GUEST => {
                        if sg_len(&iovs) < b.size {
                            return Err(resp::ERR_INVALID_PARAMETER);
                        }
                        ResKind::GuestBlob { size: b.size }
                    }
                    blob_mem::HOST3D | blob_mem::HOST3D_GUEST => {
                        self.renderer.as_mut().ok_or(resp::ERR_INVALID_PARAMETER)?.create_blob(hdr.ctx_id, b.resource_id, &b, &iovs)?;
                        ResKind::Renderer { size: b.size }
                    }
                    _ => return Err(resp::ERR_INVALID_PARAMETER),
                };
                self.resources.insert(b.resource_id, Resource { width: 0, height: 0, format: 0, kind, backing: iovs, mapped: None });
                ok(w)
            }
            cmd::RESOURCE_MAP_BLOB => {
                let m: ResourceMapBlob = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let (base, size, mapper) = self.hostmem.clone().ok_or(resp::ERR_UNSPEC)?;
                let res = self.resources.get_mut(&m.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                if !matches!(res.kind, ResKind::Renderer { .. }) || res.mapped.is_some() {
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                let rd = self.renderer.as_mut().ok_or(resp::ERR_UNSPEC)?;
                let (ptr, len) = rd.map_blob(m.resource_id)?;
                let page = sys::host_page_size() as u64;
                let len = apex_core::align_up(len, page);
                if ptr as u64 % page != 0 || m.offset % page != 0 || m.offset.checked_add(len).is_none_or(|e| e > size) {
                    rd.unmap_blob(m.resource_id);
                    return Err(resp::ERR_INVALID_PARAMETER);
                }
                // SAFETY: the renderer keeps the mapping alive until unmap_blob.
                if unsafe { mapper.map(ptr, base + m.offset, len) }.is_err() {
                    rd.unmap_blob(m.resource_id);
                    return Err(resp::ERR_UNSPEC);
                }
                let info = rd.map_info(m.resource_id);
                res.mapped = Some((m.offset, len));
                let body = RespMapInfo { map_info: info, padding: 0 };
                Ok((Self::reply_with(w, hdr, resp::OK_MAP_INFO, &body), Wait::None))
            }
            cmd::RESOURCE_UNMAP_BLOB => {
                let c: ResourceId = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let res = self.resources.get_mut(&c.resource_id).ok_or(resp::ERR_INVALID_RESOURCE_ID)?;
                let (off, len) = res.mapped.take().ok_or(resp::ERR_INVALID_PARAMETER)?;
                self.unmap_hostmem(off, len);
                if let Some(rd) = self.renderer.as_mut() {
                    rd.unmap_blob(c.resource_id);
                }
                ok(w)
            }
            cmd::CTX_CREATE => {
                let c: CtxCreate = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let rd = self.renderer.as_mut().ok_or(resp::ERR_INVALID_CONTEXT_ID)?;
                let n = (c.nlen as usize).min(64);
                rd.context_create(hdr.ctx_id, &c.debug_name[..n], c.context_init)?;
                ok(w)
            }
            cmd::CTX_DESTROY => {
                let rd = self.renderer.as_mut().ok_or(resp::ERR_INVALID_CONTEXT_ID)?;
                rd.context_destroy(hdr.ctx_id);
                self.timelines.retain(|t, q| !matches!(t, Timeline::Ring(c, _) if *c == hdr.ctx_id) || !q.is_empty());
                ok(w)
            }
            cmd::CTX_ATTACH_RESOURCE | cmd::CTX_DETACH_RESOURCE => {
                let c: ResourceId = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let rd = self.renderer.as_mut().ok_or(resp::ERR_INVALID_CONTEXT_ID)?;
                if hdr.type_ == cmd::CTX_ATTACH_RESOURCE {
                    rd.context_attach_resource(hdr.ctx_id, c.resource_id);
                } else {
                    rd.context_detach_resource(hdr.ctx_id, c.resource_id);
                }
                ok(w)
            }
            cmd::RESOURCE_CREATE_3D => {
                let c: ResourceCreate3d = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                if c.resource_id == 0 || self.resources.contains_key(&c.resource_id) {
                    return Err(resp::ERR_INVALID_RESOURCE_ID);
                }
                self.renderer.as_mut().ok_or(resp::ERR_UNSPEC)?.resource_create_3d(&c, &[])?;
                let size = c.width as u64 * c.height.max(1) as u64 * 4;
                self.resources.insert(
                    c.resource_id,
                    Resource {
                        width: c.width,
                        height: c.height,
                        format: c.format,
                        kind: ResKind::Renderer { size },
                        backing: Vec::new(),
                        mapped: None,
                    },
                );
                ok(w)
            }
            cmd::TRANSFER_TO_HOST_3D | cmd::TRANSFER_FROM_HOST_3D => {
                let t: Transfer3d = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let rd = self.renderer.as_mut().ok_or(resp::ERR_UNSPEC)?;
                if hdr.type_ == cmd::TRANSFER_TO_HOST_3D {
                    rd.transfer_write(hdr.ctx_id, &t)?;
                } else {
                    rd.transfer_read(hdr.ctx_id, &t, None)?;
                }
                Ok((Self::reply(w, hdr, resp::OK_NODATA), Wait::Renderer))
            }
            cmd::SUBMIT_3D => {
                let s: CmdSubmit = r.read_obj().map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                let mut buf = vec![0u8; s.size as usize];
                r.read_exact(&mut buf).map_err(|_| resp::ERR_INVALID_PARAMETER)?;
                self.renderer.as_mut().ok_or(resp::ERR_INVALID_CONTEXT_ID)?.submit(hdr.ctx_id, &mut buf)?;
                Ok((Self::reply(w, hdr, resp::OK_NODATA), Wait::Renderer))
            }
            _ => {
                apex_core::debug!("virtio-gpu: unsupported command {:#x}", hdr.type_);
                Err(resp::ERR_UNSPEC)
            }
        }
    }

    fn unmap_hostmem(&self, off: u64, len: u64) {
        if let Some((base, _, mapper)) = &self.hostmem {
            if let Err(e) = mapper.unmap(base + off, len) {
                apex_core::warn!("virtio-gpu: unmap host blob failed: {e}");
            }
        }
    }

    /// Compose the current scanout into a new display frame.
    fn present(&mut self) {
        let sc = self.scanout;
        let Some(res) = self.resources.get(&sc.resource_id) else { return };
        let (fmt, stride, offset, src_w) = match sc.blob {
            Some(b) => (b.format, b.strides[0] as usize, b.offsets[0] as usize, b.width),
            None => (res.format, res.width as usize * 4, 0, res.width),
        };
        let Some((layout, opaque)) = format_layout(fmt) else { return };
        let (w, h) = (sc.rect.width, sc.rect.height);
        let _ = src_w;
        let Some(mut frame) = self.display.begin_frame(w, h, layout, opaque) else { return };
        let row_bytes = w as usize * 4;
        let origin = offset + sc.rect.y as usize * stride + sc.rect.x as usize * 4;
        let ok = match &res.kind {
            ResKind::Classic { data } => {
                for y in 0..h {
                    let s = origin + y as usize * stride;
                    frame.row_mut(y).copy_from_slice(&data[s..s + row_bytes]);
                }
                true
            }
            ResKind::GuestBlob { .. } => (0..h).all(|y| sg_copy(&res.backing, origin + y as usize * stride, frame.row_mut(y))),
            ResKind::Renderer { .. } => {
                let fstride = frame.stride;
                let dst = Iov { base: frame.as_mut_ptr(), len: frame.len() };
                let t = Transfer3d {
                    box_: Box3d { x: sc.rect.x, y: sc.rect.y, z: 0, w, h, d: 1 },
                    offset: 0,
                    resource_id: sc.resource_id,
                    level: 0,
                    stride: fstride,
                    layer_stride: 0,
                };
                self.renderer.as_mut().map(|rd| rd.transfer_read(0, &t, Some(dst)).is_ok()).unwrap_or(false)
            }
        };
        if ok {
            frame.commit();
            self.frames += 1;
        }
    }

    fn release_ready(&mut self) -> bool {
        let mut any = false;
        let mem = self.mem.clone();
        let q = &mut self.queues[CTRLQ];
        for tl in self.timelines.values_mut() {
            while let Some(h) = tl.front() {
                if h.wait != Wait::None {
                    break;
                }
                let h = tl.pop_front().unwrap();
                let _ = q.add_used(&mem, h.head, h.len);
                any = true;
            }
        }
        any
    }

    fn on_vsync(&mut self) {
        for tl in self.timelines.values_mut() {
            for h in tl.iter_mut() {
                if h.wait == Wait::Vsync {
                    h.wait = Wait::None;
                }
            }
        }
    }

    fn on_fence(&mut self, f: FenceId) {
        let key = match f.ring_idx {
            Some(r) => Timeline::Ring(f.ctx_id, r),
            None => Timeline::Global,
        };
        if let Some(tl) = self.timelines.get_mut(&key) {
            for h in tl.iter_mut() {
                if h.fence_id <= f.fence_id && h.wait == Wait::Renderer {
                    h.wait = Wait::None;
                }
            }
        }
    }

    /// One pass over queues and pending events.
    fn run_once(&mut self) {
        let mem = self.mem.clone();
        let mut used = false;
        while let Some(chain) = self.queues[CTRLQ].pop(&mem) {
            match self.handle(&chain) {
                Completion::Now(len) => {
                    let _ = self.queues[CTRLQ].add_used(&mem, chain.head, len);
                    used = true;
                }
                Completion::Fenced { len, timeline, fence_id, wait } => {
                    self.timelines.entry(timeline).or_default().push_back(Held { head: chain.head, len, fence_id, wait });
                }
            }
        }
        let signalled = std::mem::take(&mut *self.shared.signalled.lock().unwrap());
        for f in signalled {
            self.on_fence(f);
        }
        let v = self.shared.vsync_seq.load(Ordering::Acquire);
        if v != self.seen_vsync {
            self.seen_vsync = v;
            self.on_vsync();
        }
        used |= self.release_ready();
        if used && self.queues[CTRLQ].needs_notification(&mem) {
            self.irq.signal_used_queue();
        }
        // Cursor queue: phones have no cursor; just return the buffers.
        let mut cused = false;
        if self.queues.len() > CURSORQ && self.queues[CURSORQ].ready {
            while let Some(c) = self.queues[CURSORQ].pop(&mem) {
                let _ = self.queues[CURSORQ].add_used(&mem, c.head, 0);
                cused = true;
            }
            if cused && self.queues[CURSORQ].needs_notification(&mem) {
                self.irq.signal_used_queue();
            }
        }
    }
}

pub struct Gpu {
    opts: Arc<GpuOptions>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    edid: [u8; 128],
    features: u64,
    events_read: u32,
    num_capsets: u32,
}

impl Gpu {
    pub fn new(opts: GpuOptions) -> Result<Gpu> {
        let cfg = opts.display.config();
        let (wmm, hmm) = cfg.physical_mm();
        let timing = edid::Timing::reduced_blanking(cfg.width, cfg.height, cfg.refresh_hz);
        let edid = edid::build(&cfg.name, opts.edid_serial, &timing, wmm, hmm).ok_or_else(|| {
            Error::Config(format!("display mode {}x{}@{} cannot be described in EDID", cfg.width, cfg.height, cfg.refresh_hz))
        })?;
        let mut features = feature::EDID | feature::RESOURCE_BLOB;
        let mut num_capsets = 0;
        if opts.renderer.is_some() {
            features |= feature::VIRGL | feature::CONTEXT_INIT;
            num_capsets = 3;
        }
        let shared =
            Arc::new(Shared { kick: Event::new(), stop: StopFlag::new(), vsync_seq: AtomicU64::new(0), signalled: Mutex::new(Vec::new()) });
        let weak = Arc::downgrade(&shared);
        opts.display.add_vsync_listener(Box::new(move |n| {
            if let Some(s) = weak.upgrade() {
                s.vsync_seq.store(n, Ordering::Release);
                s.kick.signal();
            }
        }));
        Ok(Gpu { opts: Arc::new(opts), shared, thread: None, edid, features, events_read: 0, num_capsets })
    }

    fn stop_worker(&mut self) {
        if let Some(t) = self.thread.take() {
            self.shared.stop.stop();
            self.shared.kick.signal();
            let _ = t.join();
            self.shared.stop.reset();
        }
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        device_type::GPU
    }
    fn name(&self) -> &str {
        "gpu"
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 16]
    }
    fn device_features(&self) -> u64 {
        self.features
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 16];
        cfg[0..4].copy_from_slice(&self.events_read.to_le_bytes());
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        cfg[12..16].copy_from_slice(&self.num_capsets.to_le_bytes());
        super::read_config_bytes(&cfg, offset, data)
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if offset == 4 && data.len() == 4 {
            self.events_read &= !u32::from_le_bytes(data.try_into().unwrap());
        }
    }
    fn shm_regions(&self) -> Vec<ShmRegion> {
        match &self.opts.hostmem {
            Some((base, len, _)) if self.opts.renderer.is_some() => vec![ShmRegion { id: SHM_HOST_VISIBLE, base: *base, len: *len }],
            _ => Vec::new(),
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.stop_worker();
        let opts = self.opts.clone();
        let shared = self.shared.clone();
        let edid = self.edid;
        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let t = std::thread::Builder::new()
            .name("virtio-gpu".into())
            .spawn(move || {
                sys::set_thread_latency_critical();
                let sig = Arc::downgrade(&shared);
                let renderer = match &opts.renderer {
                    Some(factory) => {
                        let sink: renderer::FenceSink = Arc::new(move |f| {
                            if let Some(s) = sig.upgrade() {
                                s.signalled.lock().unwrap().push(f);
                                s.kick.signal();
                            }
                        });
                        match factory(sink) {
                            Ok(r) => Some(r),
                            Err(e) => {
                                apex_core::error!("3D renderer unavailable, continuing with 2D only: {e}");
                                None
                            }
                        }
                    }
                    None => None,
                };
                let _ = tx.send(Ok(()));
                let mut w = Worker {
                    mem: ctx.mem,
                    queues: ctx.queues,
                    irq: ctx.interrupt,
                    display: opts.display.clone(),
                    renderer,
                    hostmem: opts.hostmem.clone(),
                    pace: opts.pace_flushes,
                    edid,
                    resources: HashMap::new(),
                    scanout: Scanout::default(),
                    timelines: HashMap::new(),
                    seen_vsync: shared.vsync_seq.load(Ordering::Acquire),
                    bytes_2d: 0,
                    shared: shared.clone(),
                    frames: 0,
                };
                while !shared.stop.is_stopped() {
                    w.run_once();
                    shared.kick.wait();
                }
                // Unmap host blobs before the renderer goes away.
                let mapped: Vec<(u64, u64)> = w.resources.values().filter_map(|r| r.mapped).collect();
                for (off, len) in mapped {
                    w.unmap_hostmem(off, len);
                }
            })
            .map_err(Error::Io)?;
        rx.recv().map_err(|_| Error::Device("virtio-gpu worker died during start".into()))??;
        self.thread = Some(t);
        Ok(())
    }
    fn queue_notify(&mut self, _index: u16) {
        self.shared.kick.signal();
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayConfig, PixelLayout};
    use crate::virtio::queue::test_driver::Driver;
    use apex_core::irq::{IrqLine, RecordingIrqChip};

    fn hdr(t: u32, fence: Option<u64>) -> Vec<u8> {
        let h =
            CtrlHdr { type_: t, flags: if fence.is_some() { FLAG_FENCE } else { 0 }, fence_id: fence.unwrap_or(0), ..Default::default() };
        h.as_bytes().to_vec()
    }

    fn cmd<T: ByteValued>(t: u32, fence: Option<u64>, body: &T) -> Vec<u8> {
        let mut v = hdr(t, fence);
        v.extend_from_slice(body.as_bytes());
        v
    }

    fn worker(drv: &Driver, display: Arc<DisplayHub>, pace: bool) -> Worker {
        let chip = Arc::new(RecordingIrqChip::default());
        let mut cq = drv.q.clone();
        cq.ready = false;
        let cfg = display.config();
        Worker {
            mem: drv.mem.clone(),
            queues: vec![drv.q.clone(), cq],
            irq: VirtioInterrupt::new(IrqLine::new(chip, 4)),
            display,
            renderer: None,
            hostmem: None,
            pace,
            edid: edid::build("t", 0, &edid::Timing::reduced_blanking(cfg.width, cfg.height, 120), 65, 145).unwrap(),
            resources: HashMap::new(),
            scanout: Scanout::default(),
            timelines: HashMap::new(),
            seen_vsync: 0,
            bytes_2d: 0,
            shared: Arc::new(Shared {
                kick: Event::new(),
                stop: StopFlag::new(),
                vsync_seq: AtomicU64::new(0),
                signalled: Mutex::new(Vec::new()),
            }),
            frames: 0,
        }
    }

    fn resp_type(drv: &Driver, a: GuestAddress) -> u32 {
        u32::from_le_bytes(drv.read(a, 4).try_into().unwrap())
    }

    #[test]
    fn display_info_and_edid() {
        let mut drv = Driver::new(32);
        let hub = DisplayHub::new(DisplayConfig::phone_default());
        let (_, di) = drv.add_chain(&[&hdr(cmd::GET_DISPLAY_INFO, None)], &[24 + 24 * 16]);
        let (_, ed) = drv.add_chain(&[&cmd(cmd::GET_EDID, None, &GetEdid::default())], &[24 + 8 + 1024]);
        let mut w = worker(&drv, hub, true);
        drv.q = w.queues[0].clone();
        w.run_once();
        assert_eq!(resp_type(&drv, di[0]), resp::OK_DISPLAY_INFO);
        let pm = drv.read(di[0].unchecked_add(24), 24);
        assert_eq!(u32::from_le_bytes(pm[8..12].try_into().unwrap()), 1080);
        assert_eq!(u32::from_le_bytes(pm[12..16].try_into().unwrap()), 2400);
        assert_eq!(resp_type(&drv, ed[0]), resp::OK_EDID);
        assert_eq!(drv.read(ed[0].unchecked_add(32), 8), vec![0, 255, 255, 255, 255, 255, 255, 0]);
    }

    #[test]
    fn classic_2d_flush_is_paced_by_vsync() {
        let mut drv = Driver::new(64);
        let mut cfg = DisplayConfig::phone_default();
        cfg.width = 64;
        cfg.height = 32;
        let hub = DisplayHub::new(cfg);
        // Guest framebuffer backing with a recognizable pattern.
        let fb = drv.alloc(64 * 32 * 4);
        let mut pixels = vec![0u8; 64 * 32 * 4];
        for (i, p) in pixels.chunks_mut(4).enumerate() {
            p.copy_from_slice(&[(i % 251) as u8, 0x22, 0x33, 0xff]);
        }
        drv.mem.write(&pixels, fb).unwrap();

        let create = ResourceCreate2d { resource_id: 7, format: format::B8G8R8X8_UNORM, width: 64, height: 32 };
        drv.add_chain(&[&cmd(cmd::RESOURCE_CREATE_2D, None, &create)], &[24]);
        let mut attach = cmd(cmd::RESOURCE_ATTACH_BACKING, None, &AttachBacking { resource_id: 7, nr_entries: 2 });
        attach.extend_from_slice(MemEntry { addr: fb.0, length: 4096, padding: 0 }.as_bytes());
        attach.extend_from_slice(MemEntry { addr: fb.0 + 4096, length: 64 * 32 * 4 - 4096, padding: 0 }.as_bytes());
        drv.add_chain(&[&attach], &[24]);
        let full = Rect { x: 0, y: 0, width: 64, height: 32 };
        drv.add_chain(&[&cmd(cmd::SET_SCANOUT, None, &SetScanout { r: full, scanout_id: 0, resource_id: 7 })], &[24]);
        drv.add_chain(&[&cmd(cmd::TRANSFER_TO_HOST_2D, None, &TransferToHost2d { r: full, offset: 0, resource_id: 7, padding: 0 })], &[24]);
        let (flush_head, flush_resp) =
            drv.add_chain(&[&cmd(cmd::RESOURCE_FLUSH, Some(9), &ResourceFlush { r: full, resource_id: 7, padding: 0 })], &[24]);
        // A later fenced command must not overtake the held flush fence.
        let (later_head, _) =
            drv.add_chain(&[&cmd(cmd::RESOURCE_FLUSH, Some(10), &ResourceFlush { r: full, resource_id: 99, padding: 0 })], &[24]);

        let mut w = worker(&drv, hub.clone(), true);
        w.run_once();
        let used: Vec<u16> = drv.take_used().into_iter().map(|u| u.0).collect();
        assert_eq!(used.len(), 4, "only unfenced commands complete before vsync");
        assert!(!used.contains(&flush_head) && !used.contains(&later_head));

        // Frame content reached the swapchain already.
        let f = hub.acquire(0).unwrap();
        assert_eq!((f.width, f.height), (64, 32));
        let row1 = unsafe { std::slice::from_raw_parts(f.data.add(f.stride as usize), 8) };
        assert_eq!(row1, &[64, 0x22, 0x33, 0xff, 65, 0x22, 0x33, 0xff]);
        hub.release(f.slot);

        // Vsync releases both, in order.
        w.shared.vsync_seq.store(1, Ordering::Release);
        w.run_once();
        let used: Vec<u16> = drv.take_used().into_iter().map(|u| u.0).collect();
        assert_eq!(used, vec![flush_head, later_head]);
        let h: CtrlHdr = drv.mem.read_obj(flush_resp[0]).unwrap();
        assert_eq!((h.type_, h.flags & FLAG_FENCE, h.fence_id), (resp::OK_NODATA, FLAG_FENCE, 9));
    }

    #[test]
    fn guest_blob_scanout_zero_copy_source() {
        let mut drv = Driver::new(32);
        let mut cfg = DisplayConfig::phone_default();
        cfg.width = 16;
        cfg.height = 8;
        let hub = DisplayHub::new(cfg);
        let stride = 16 * 4 + 64; // padded guest stride
        let blob = drv.alloc(stride * 8);
        drv.mem.fill(blob, (stride * 8) as u64, 0x5a).unwrap();
        let mut create = cmd(
            cmd::RESOURCE_CREATE_BLOB,
            None,
            &ResourceCreateBlob {
                resource_id: 3,
                blob_mem: blob_mem::GUEST,
                blob_flags: blob_flag::USE_SHAREABLE,
                nr_entries: 1,
                blob_id: 0,
                size: (stride * 8) as u64,
            },
        );
        create.extend_from_slice(MemEntry { addr: blob.0, length: (stride * 8) as u32, padding: 0 }.as_bytes());
        drv.add_chain(&[&create], &[24]);
        let mut sb = SetScanoutBlob {
            r: Rect { x: 0, y: 0, width: 16, height: 8 },
            scanout_id: 0,
            resource_id: 3,
            width: 16,
            height: 8,
            format: format::R8G8B8A8_UNORM,
            ..Default::default()
        };
        sb.strides[0] = stride as u32;
        drv.add_chain(&[&cmd(cmd::SET_SCANOUT_BLOB, None, &sb)], &[24]);
        let (_, fr) = drv.add_chain(&[&cmd(cmd::RESOURCE_FLUSH, None, &ResourceFlush { r: sb.r, resource_id: 3, padding: 0 })], &[24]);
        let mut w = worker(&drv, hub.clone(), true);
        w.run_once();
        assert_eq!(resp_type(&drv, fr[0]), resp::OK_NODATA);
        let f = hub.acquire(0).unwrap();
        assert_eq!(f.layout, PixelLayout::Rgba as u32);
        assert_eq!(unsafe { *f.data.add(f.stride as usize * 7 + 63) }, 0x5a);
    }

    #[test]
    fn errors_are_reported() {
        let mut drv = Driver::new(32);
        let hub = DisplayHub::new(DisplayConfig::phone_default());
        let (_, a) = drv.add_chain(
            &[&cmd(cmd::SET_SCANOUT, None, &SetScanout { r: Rect { x: 0, y: 0, width: 1, height: 1 }, scanout_id: 0, resource_id: 5 })],
            &[24],
        );
        let (_, b) = drv.add_chain(
            &[&cmd(cmd::RESOURCE_CREATE_2D, None, &ResourceCreate2d { resource_id: 1, format: 999, width: 1, height: 1 })],
            &[24],
        );
        let (_, c) = drv.add_chain(&[&hdr(0x9999, None)], &[24]);
        let (_, d) = drv.add_chain(&[&cmd(cmd::CTX_CREATE, None, &CtxCreate::default())], &[24]);
        let mut w = worker(&drv, hub, true);
        w.run_once();
        assert_eq!(resp_type(&drv, a[0]), resp::ERR_INVALID_RESOURCE_ID);
        assert_eq!(resp_type(&drv, b[0]), resp::ERR_INVALID_PARAMETER);
        assert_eq!(resp_type(&drv, c[0]), resp::ERR_UNSPEC);
        assert_eq!(resp_type(&drv, d[0]), resp::ERR_INVALID_CONTEXT_ID);
    }
}
