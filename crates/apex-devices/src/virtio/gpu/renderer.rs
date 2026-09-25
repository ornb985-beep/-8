//! 3D renderer interface behind virtio-gpu (VIRGL/CONTEXT_INIT commands).
//!
//! The production back end is gfxstream (the same host renderer the Android
//! Emulator uses; on macOS it runs guest GLES/Vulkan on Metal through
//! MoltenVK/ANGLE). It is loaded at runtime, see [`super::gfxstream`].

use std::sync::Arc;

use super::protocol::{ResourceCreate3d, ResourceCreateBlob, Transfer3d};

/// A host view of guest (or host) memory.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Iov {
    pub base: *mut u8,
    pub len: usize,
}

// SAFETY: plain pointers into guest RAM, whose lifetime is the VM's.
unsafe impl Send for Iov {}
unsafe impl Sync for Iov {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FenceId {
    pub ctx_id: u32,
    /// Some(ring) for per-context timelines (CONTEXT_INIT), None for the
    /// global timeline.
    pub ring_idx: Option<u8>,
    pub fence_id: u64,
}

pub type FenceSink = Arc<dyn Fn(FenceId) + Send + Sync>;

/// Error value is a virtio-gpu response code (`resp::ERR_*`).
pub type RResult<T> = std::result::Result<T, u32>;

pub trait Renderer3d: Send {
    fn name(&self) -> &str;
    /// (capset id, max version, max size)
    fn capsets(&self) -> Vec<(u32, u32, u32)>;
    fn fill_caps(&self, id: u32, version: u32) -> Vec<u8>;
    fn context_create(&mut self, ctx_id: u32, name: &[u8], context_init: u32) -> RResult<()>;
    fn context_destroy(&mut self, ctx_id: u32);
    fn context_attach_resource(&mut self, ctx_id: u32, res: u32);
    fn context_detach_resource(&mut self, ctx_id: u32, res: u32);
    fn resource_create_3d(&mut self, args: &ResourceCreate3d, iovs: &[Iov]) -> RResult<()>;
    fn resource_unref(&mut self, res: u32);
    fn attach_backing(&mut self, res: u32, iovs: &[Iov]) -> RResult<()>;
    fn detach_backing(&mut self, res: u32);
    fn transfer_write(&mut self, ctx_id: u32, t: &Transfer3d) -> RResult<()>;
    /// Read back into the resource backing, or into `dst` if given (used
    /// to present 3D scanouts into the display swapchain).
    fn transfer_read(&mut self, ctx_id: u32, t: &Transfer3d, dst: Option<Iov>) -> RResult<()>;
    fn submit(&mut self, ctx_id: u32, cmd: &mut [u8]) -> RResult<()>;
    /// Ask the renderer to signal `fence` (through the fence sink) once all
    /// prior work on that timeline completed.
    fn create_fence(&mut self, fence: FenceId) -> RResult<()>;
    fn create_blob(&mut self, ctx_id: u32, res: u32, blob: &ResourceCreateBlob, iovs: &[Iov]) -> RResult<()>;
    /// Host pointer + size of a host-visible blob (mapped into the guest's
    /// shared memory window by the device).
    fn map_blob(&mut self, res: u32) -> RResult<(*mut u8, u64)>;
    fn unmap_blob(&mut self, res: u32);
    fn map_info(&mut self, res: u32) -> u32;
}

/// Maps host memory into the guest physical address space (implemented by
/// the VMM on top of `hv_vm_map`).
pub trait HostMapper: Send + Sync {
    /// # Safety
    /// `host` must stay valid until `unmap`.
    unsafe fn map(&self, host: *mut u8, gpa: u64, size: u64) -> apex_core::Result<()>;
    fn unmap(&self, gpa: u64, size: u64) -> apex_core::Result<()>;
}
