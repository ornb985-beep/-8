#![allow(clippy::missing_transmute_annotations)]
//! gfxstream host renderer, loaded with `dlopen` from
//! `libgfxstream_backend.{dylib,so}` through its stable C ABI
//! (`host/include/gfxstream/virtio-gpu-gfxstream-renderer.h`, API 0.1.2).
//!
//! Loading at runtime keeps the VMM buildable without the (large, C++)
//! renderer and lets users drop in a build matching their macOS/MoltenVK.

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::Mutex;

use apex_core::{Error, Result};

use super::protocol::{resp, ResourceCreate3d, ResourceCreateBlob, Transfer3d};
use super::renderer::{FenceId, FenceSink, Iov, RResult, Renderer3d};

#[repr(C)]
struct Param {
    key: u64,
    value: u64,
}

#[repr(C)]
struct ResourceCreateArgs {
    handle: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
}

#[repr(C)]
struct SrBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
struct SrFence {
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
}

#[repr(C)]
struct SrHandle {
    os_handle: i64,
    handle_type: u32,
}

#[repr(C)]
struct SrCommand {
    ctx_id: u32,
    cmd_size: u32,
    cmd: *mut u8,
    num_in_fences: u32,
    fences: *mut SrHandle,
}

#[repr(C)]
struct SrCreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
}

const PARAM_USER_DATA: u64 = 1;
const PARAM_RENDERER_FLAGS: u64 = 2;
const PARAM_FENCE_CALLBACK: u64 = 3;
const PARAM_WIN0_WIDTH: u64 = 4;
const PARAM_WIN0_HEIGHT: u64 = 5;

pub mod flags {
    pub const USE_EGL: u64 = 1 << 0;
    pub const THREAD_SYNC: u64 = 1 << 1;
    pub const USE_SURFACELESS: u64 = 1 << 3;
    pub const USE_GLES: u64 = 1 << 4;
    pub const USE_VK: u64 = 1 << 5;
    pub const USE_EXTERNAL_BLOB: u64 = 1 << 6;
    pub const USE_SYSTEM_BLOB: u64 = 1 << 7;
    pub const VULKAN_NATIVE_SWAPCHAIN: u64 = 1 << 8;
}

const FLAG_FENCE: u32 = 1;
const FLAG_FENCE_RING_IDX: u32 = 2;

type FenceCb = extern "C" fn(*mut c_void, *mut SrFence);

struct Api {
    init: unsafe extern "C" fn(*mut Param, u64) -> c_int,
    teardown: unsafe extern "C" fn(),
    resource_create: unsafe extern "C" fn(*mut ResourceCreateArgs, *mut Iov, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    context_destroy: unsafe extern "C" fn(u32),
    submit_cmd: unsafe extern "C" fn(*mut SrCommand) -> c_int,
    transfer_read_iov: unsafe extern "C" fn(u32, u32, u32, u32, u32, *mut SrBox, u64, *mut Iov, c_int) -> c_int,
    transfer_write_iov: unsafe extern "C" fn(u32, u32, c_int, u32, u32, *mut SrBox, u64, *mut Iov, u32) -> c_int,
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    resource_attach_iov: unsafe extern "C" fn(c_int, *mut Iov, c_int) -> c_int,
    resource_detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iov, *mut c_int),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    create_blob: unsafe extern "C" fn(u32, u32, *const SrCreateBlob, *const Iov, u32, *const SrHandle) -> c_int,
    resource_map: unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int,
    resource_unmap: unsafe extern "C" fn(u32) -> c_int,
    context_create: unsafe extern "C" fn(u32, u32, *const c_char, u32) -> c_int,
    create_fence: unsafe extern "C" fn(*const SrFence) -> c_int,
    resource_map_info: unsafe extern "C" fn(u32, *mut u32) -> c_int,
}

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

const RTLD_NOW: c_int = 2;
#[cfg(target_os = "macos")]
const RTLD_LOCAL: c_int = 4;
#[cfg(not(target_os = "macos"))]
const RTLD_LOCAL: c_int = 0;

fn last_dl_error() -> String {
    // SAFETY: dlerror returns a thread-local C string or NULL.
    unsafe {
        let p = dlerror();
        if p.is_null() {
            "unknown dlopen error".into()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Only one renderer may exist per process (gfxstream is a singleton).
static INSTANCE: Mutex<bool> = Mutex::new(false);

struct CallbackCtx {
    sink: FenceSink,
}

extern "C" fn on_fence(user: *mut c_void, f: *mut SrFence) {
    if user.is_null() || f.is_null() {
        return;
    }
    // SAFETY: `user` is the CallbackCtx we leaked in `load`, `f` is valid
    // for the duration of the callback.
    let (ctx, fence) = unsafe { (&*(user as *const CallbackCtx), &*f) };
    let ring = (fence.flags & FLAG_FENCE_RING_IDX != 0).then_some(fence.ring_idx);
    (ctx.sink)(FenceId { ctx_id: fence.ctx_id, ring_idx: ring, fence_id: fence.fence_id });
}

pub struct Gfxstream {
    api: Api,
    _cb: Box<CallbackCtx>,
}

// SAFETY: gfxstream's C API is internally synchronized; we additionally only
// call it from the virtio-gpu worker thread.
unsafe impl Send for Gfxstream {}

impl Gfxstream {
    pub fn load(path: &str, width: u32, height: u32, renderer_flags: u64, sink: FenceSink) -> Result<Gfxstream> {
        let mut inst = INSTANCE.lock().unwrap();
        if *inst {
            return Err(Error::Device("gfxstream already initialized in this process".into()));
        }
        let cpath = CString::new(path).map_err(|_| Error::Config("bad gfxstream path".into()))?;
        // SAFETY: plain dlopen of a user-provided library path.
        let h = unsafe { dlopen(cpath.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if h.is_null() {
            return Err(Error::Device(format!("dlopen {path}: {}", last_dl_error())));
        }
        macro_rules! sym {
            ($name:literal) => {{
                let n = CString::new($name).unwrap();
                // SAFETY: symbol lookup in a library we just opened.
                let p = unsafe { dlsym(h, n.as_ptr()) };
                if p.is_null() {
                    return Err(Error::Device(format!("gfxstream: missing symbol {}", $name)));
                }
                // SAFETY: the symbol has the C signature declared in `Api`
                // (checked against the gfxstream header, API 0.1.2).
                unsafe { std::mem::transmute(p) }
            }};
        }
        let api = Api {
            init: sym!("stream_renderer_init"),
            teardown: sym!("stream_renderer_teardown"),
            resource_create: sym!("stream_renderer_resource_create"),
            resource_unref: sym!("stream_renderer_resource_unref"),
            context_destroy: sym!("stream_renderer_context_destroy"),
            submit_cmd: sym!("stream_renderer_submit_cmd"),
            transfer_read_iov: sym!("stream_renderer_transfer_read_iov"),
            transfer_write_iov: sym!("stream_renderer_transfer_write_iov"),
            get_cap_set: sym!("stream_renderer_get_cap_set"),
            fill_caps: sym!("stream_renderer_fill_caps"),
            resource_attach_iov: sym!("stream_renderer_resource_attach_iov"),
            resource_detach_iov: sym!("stream_renderer_resource_detach_iov"),
            ctx_attach_resource: sym!("stream_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!("stream_renderer_ctx_detach_resource"),
            create_blob: sym!("stream_renderer_create_blob"),
            resource_map: sym!("stream_renderer_resource_map"),
            resource_unmap: sym!("stream_renderer_resource_unmap"),
            context_create: sym!("stream_renderer_context_create"),
            create_fence: sym!("stream_renderer_create_fence"),
            resource_map_info: sym!("stream_renderer_resource_map_info"),
        };
        let cb = Box::new(CallbackCtx { sink });
        let fence_cb: FenceCb = on_fence;
        let mut params = [
            Param { key: PARAM_USER_DATA, value: &*cb as *const CallbackCtx as u64 },
            Param { key: PARAM_RENDERER_FLAGS, value: renderer_flags },
            Param { key: PARAM_FENCE_CALLBACK, value: fence_cb as usize as u64 },
            Param { key: PARAM_WIN0_WIDTH, value: width as u64 },
            Param { key: PARAM_WIN0_HEIGHT, value: height as u64 },
        ];
        // SAFETY: parameters follow the stream_renderer_param contract; the
        // callback context outlives the renderer (owned by `Gfxstream`).
        let rc = unsafe { (api.init)(params.as_mut_ptr(), params.len() as u64) };
        if rc != 0 {
            return Err(Error::Device(format!("stream_renderer_init failed: {rc}")));
        }
        *inst = true;
        apex_core::info!("gfxstream renderer loaded from {path}");
        Ok(Gfxstream { api, _cb: cb })
    }
}

impl Drop for Gfxstream {
    fn drop(&mut self) {
        // SAFETY: initialized in `load`.
        unsafe { (self.api.teardown)() };
        *INSTANCE.lock().unwrap() = false;
    }
}

fn check(rc: c_int) -> RResult<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(resp::ERR_UNSPEC)
    }
}

fn to_box(t: &Transfer3d) -> SrBox {
    SrBox { x: t.box_.x, y: t.box_.y, z: t.box_.z, w: t.box_.w, h: t.box_.h, d: t.box_.d }
}

impl Renderer3d for Gfxstream {
    fn name(&self) -> &str {
        "gfxstream"
    }

    fn capsets(&self) -> Vec<(u32, u32, u32)> {
        use super::protocol::capset::*;
        let mut v = Vec::new();
        for id in [GFXSTREAM_VULKAN, GFXSTREAM_GLES, GFXSTREAM_COMPOSER, GFXSTREAM_MAGMA] {
            let (mut ver, mut size) = (0u32, 0u32);
            // SAFETY: out-pointers are valid.
            unsafe { (self.api.get_cap_set)(id, &mut ver, &mut size) };
            if size > 0 {
                v.push((id, ver, size));
            }
        }
        v
    }

    fn fill_caps(&self, id: u32, version: u32) -> Vec<u8> {
        let (mut ver, mut size) = (0u32, 0u32);
        // SAFETY: out-pointers are valid.
        unsafe { (self.api.get_cap_set)(id, &mut ver, &mut size) };
        let mut buf = vec![0u8; size as usize];
        if size > 0 {
            // SAFETY: buffer sized as reported by the renderer.
            unsafe { (self.api.fill_caps)(id, version, buf.as_mut_ptr() as *mut c_void) };
        }
        buf
    }

    fn context_create(&mut self, ctx_id: u32, name: &[u8], context_init: u32) -> RResult<()> {
        // SAFETY: name pointer/length pair is valid.
        check(unsafe { (self.api.context_create)(ctx_id, name.len() as u32, name.as_ptr() as *const c_char, context_init) })
    }

    fn context_destroy(&mut self, ctx_id: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.context_destroy)(ctx_id) }
    }

    fn context_attach_resource(&mut self, ctx_id: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_attach_resource)(ctx_id as c_int, res as c_int) }
    }

    fn context_detach_resource(&mut self, ctx_id: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_detach_resource)(ctx_id as c_int, res as c_int) }
    }

    fn resource_create_3d(&mut self, a: &ResourceCreate3d, iovs: &[Iov]) -> RResult<()> {
        let mut args = ResourceCreateArgs {
            handle: a.resource_id,
            target: a.target,
            format: a.format,
            bind: a.bind,
            width: a.width,
            height: a.height,
            depth: a.depth,
            array_size: a.array_size,
            last_level: a.last_level,
            nr_samples: a.nr_samples,
            flags: a.flags,
        };
        let mut v = iovs.to_vec();
        // SAFETY: iovecs describe guest memory valid for the VM lifetime.
        check(unsafe { (self.api.resource_create)(&mut args, v.as_mut_ptr(), v.len() as u32) })
    }

    fn resource_unref(&mut self, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.resource_unref)(res) }
    }

    fn attach_backing(&mut self, res: u32, iovs: &[Iov]) -> RResult<()> {
        let mut v = iovs.to_vec();
        // SAFETY: see resource_create_3d.
        check(unsafe { (self.api.resource_attach_iov)(res as c_int, v.as_mut_ptr(), v.len() as c_int) })
    }

    fn detach_backing(&mut self, res: u32) {
        let mut p: *mut Iov = std::ptr::null_mut();
        let mut n: c_int = 0;
        // SAFETY: out-pointers valid; the returned array is owned by gfxstream
        // and only describes guest memory we own, so nothing needs freeing.
        unsafe { (self.api.resource_detach_iov)(res as c_int, &mut p, &mut n) }
    }

    fn transfer_write(&mut self, ctx_id: u32, t: &Transfer3d) -> RResult<()> {
        let mut b = to_box(t);
        // SAFETY: NULL iovecs = use the attached backing.
        check(unsafe {
            (self.api.transfer_write_iov)(
                t.resource_id,
                ctx_id,
                t.level as c_int,
                t.stride,
                t.layer_stride,
                &mut b,
                t.offset,
                std::ptr::null_mut(),
                0,
            )
        })
    }

    fn transfer_read(&mut self, ctx_id: u32, t: &Transfer3d, dst: Option<Iov>) -> RResult<()> {
        let mut b = to_box(t);
        let mut iov = dst;
        let (p, n) = match iov.as_mut() {
            Some(i) => (i as *mut Iov, 1),
            None => (std::ptr::null_mut(), 0),
        };
        // SAFETY: optional single iovec pointing at a swapchain slot.
        check(unsafe { (self.api.transfer_read_iov)(t.resource_id, ctx_id, t.level, t.stride, t.layer_stride, &mut b, t.offset, p, n) })
    }

    fn submit(&mut self, ctx_id: u32, cmd: &mut [u8]) -> RResult<()> {
        let mut c = SrCommand { ctx_id, cmd_size: cmd.len() as u32, cmd: cmd.as_mut_ptr(), num_in_fences: 0, fences: std::ptr::null_mut() };
        // SAFETY: command buffer valid for the call.
        check(unsafe { (self.api.submit_cmd)(&mut c) })
    }

    fn create_fence(&mut self, f: FenceId) -> RResult<()> {
        let fence = SrFence {
            flags: FLAG_FENCE | if f.ring_idx.is_some() { FLAG_FENCE_RING_IDX } else { 0 },
            fence_id: f.fence_id,
            ctx_id: f.ctx_id,
            ring_idx: f.ring_idx.unwrap_or(0),
        };
        // SAFETY: plain call.
        check(unsafe { (self.api.create_fence)(&fence) })
    }

    fn create_blob(&mut self, ctx_id: u32, res: u32, blob: &ResourceCreateBlob, iovs: &[Iov]) -> RResult<()> {
        let b = SrCreateBlob { blob_mem: blob.blob_mem, blob_flags: blob.blob_flags, blob_id: blob.blob_id, size: blob.size };
        let p = if iovs.is_empty() { std::ptr::null() } else { iovs.as_ptr() };
        // SAFETY: arguments valid for the call.
        check(unsafe { (self.api.create_blob)(ctx_id, res, &b, p, iovs.len() as u32, std::ptr::null()) })
    }

    fn map_blob(&mut self, res: u32) -> RResult<(*mut u8, u64)> {
        let mut p: *mut c_void = std::ptr::null_mut();
        let mut size = 0u64;
        // SAFETY: out-pointers valid.
        check(unsafe { (self.api.resource_map)(res, &mut p, &mut size) })?;
        Ok((p as *mut u8, size))
    }

    fn unmap_blob(&mut self, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.resource_unmap)(res) };
    }

    fn map_info(&mut self, res: u32) -> u32 {
        let mut info = 0u32;
        // SAFETY: out-pointer valid.
        unsafe { (self.api.resource_map_info)(res, &mut info) };
        info
    }
}
