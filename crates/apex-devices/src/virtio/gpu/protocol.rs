//! virtio-gpu wire protocol (virtio 1.2, 5.7) as plain little-endian structs.

#![allow(dead_code)]

use apex_core::mem::ByteValued;

pub mod feature {
    pub const VIRGL: u64 = 1 << 0;
    pub const EDID: u64 = 1 << 1;
    pub const RESOURCE_UUID: u64 = 1 << 2;
    pub const RESOURCE_BLOB: u64 = 1 << 3;
    pub const CONTEXT_INIT: u64 = 1 << 4;
}

pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const GET_EDID: u32 = 0x010a;
    pub const RESOURCE_ASSIGN_UUID: u32 = 0x010b;
    pub const RESOURCE_CREATE_BLOB: u32 = 0x010c;
    pub const SET_SCANOUT_BLOB: u32 = 0x010d;

    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const RESOURCE_CREATE_3D: u32 = 0x0204;
    pub const TRANSFER_TO_HOST_3D: u32 = 0x0205;
    pub const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
    pub const SUBMIT_3D: u32 = 0x0207;
    pub const RESOURCE_MAP_BLOB: u32 = 0x0208;
    pub const RESOURCE_UNMAP_BLOB: u32 = 0x0209;

    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;
}

pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
    pub const OK_EDID: u32 = 0x1104;
    pub const OK_RESOURCE_UUID: u32 = 0x1105;
    pub const OK_MAP_INFO: u32 = 0x1106;

    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

pub const FLAG_FENCE: u32 = 1 << 0;
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

pub const MAX_SCANOUTS: usize = 16;

/// Pixel formats (virtio_gpu_formats).
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
    pub const B8G8R8X8_UNORM: u32 = 2;
    pub const A8R8G8B8_UNORM: u32 = 3;
    pub const X8R8G8B8_UNORM: u32 = 4;
    pub const R8G8B8A8_UNORM: u32 = 67;
    pub const X8B8G8R8_UNORM: u32 = 68;
    pub const A8B8G8R8_UNORM: u32 = 121;
    pub const R8G8B8X8_UNORM: u32 = 134;
}

pub mod blob_mem {
    pub const GUEST: u32 = 1;
    pub const HOST3D: u32 = 2;
    pub const HOST3D_GUEST: u32 = 3;
}

pub mod blob_flag {
    pub const USE_MAPPABLE: u32 = 1;
    pub const USE_SHAREABLE: u32 = 2;
    pub const USE_CROSS_DEVICE: u32 = 4;
}

/// Capability set IDs.
pub mod capset {
    pub const VIRGL: u32 = 1;
    pub const VIRGL2: u32 = 2;
    pub const GFXSTREAM_VULKAN: u32 = 3;
    pub const VENUS: u32 = 4;
    pub const CROSS_DOMAIN: u32 = 5;
    pub const DRM: u32 = 6;
    pub const GFXSTREAM_MAGMA: u32 = 7;
    pub const GFXSTREAM_GLES: u32 = 8;
    pub const GFXSTREAM_COMPOSER: u32 = 9;
}

/// Shared memory region id for host visible blobs.
pub const SHM_HOST_VISIBLE: u8 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtrlHdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}
unsafe impl ByteValued for CtrlHdr {}

impl CtrlHdr {
    pub fn response(req: &CtrlHdr, type_: u32) -> CtrlHdr {
        let mut h = CtrlHdr { type_, ..Default::default() };
        if req.flags & FLAG_FENCE != 0 {
            h.flags = FLAG_FENCE | (req.flags & FLAG_INFO_RING_IDX);
            h.fence_id = req.fence_id;
            h.ctx_id = req.ctx_id;
            h.ring_idx = req.ring_idx;
        }
        h
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
unsafe impl ByteValued for Rect {}

impl Rect {
    pub fn within(&self, w: u32, h: u32) -> bool {
        self.x.checked_add(self.width).is_some_and(|e| e <= w) && self.y.checked_add(self.height).is_some_and(|e| e <= h)
    }
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayOne {
    pub r: Rect,
    pub enabled: u32,
    pub flags: u32,
}
unsafe impl ByteValued for DisplayOne {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreate2d {
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}
unsafe impl ByteValued for ResourceCreate2d {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceId {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for ResourceId {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetScanout {
    pub r: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}
unsafe impl ByteValued for SetScanout {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceFlush {
    pub r: Rect,
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for ResourceFlush {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TransferToHost2d {
    pub r: Rect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for TransferToHost2d {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AttachBacking {
    pub resource_id: u32,
    pub nr_entries: u32,
}
unsafe impl ByteValued for AttachBacking {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}
unsafe impl ByteValued for MemEntry {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetCapsetInfo {
    pub capset_index: u32,
    pub padding: u32,
}
unsafe impl ByteValued for GetCapsetInfo {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RespCapsetInfo {
    pub capset_id: u32,
    pub capset_max_version: u32,
    pub capset_max_size: u32,
    pub padding: u32,
}
unsafe impl ByteValued for RespCapsetInfo {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetCapset {
    pub capset_id: u32,
    pub capset_version: u32,
}
unsafe impl ByteValued for GetCapset {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetEdid {
    pub scanout: u32,
    pub padding: u32,
}
unsafe impl ByteValued for GetEdid {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreateBlob {
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}
unsafe impl ByteValued for ResourceCreateBlob {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetScanoutBlob {
    pub r: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub padding: u32,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}
unsafe impl ByteValued for SetScanoutBlob {}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CtxCreate {
    pub nlen: u32,
    pub context_init: u32,
    pub debug_name: [u8; 64],
}
unsafe impl ByteValued for CtxCreate {}

impl Default for CtxCreate {
    fn default() -> Self {
        CtxCreate { nlen: 0, context_init: 0, debug_name: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreate3d {
    pub resource_id: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub padding: u32,
}
unsafe impl ByteValued for ResourceCreate3d {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Box3d {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}
unsafe impl ByteValued for Box3d {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Transfer3d {
    pub box_: Box3d,
    pub offset: u64,
    pub resource_id: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}
unsafe impl ByteValued for Transfer3d {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CmdSubmit {
    pub size: u32,
    pub num_in_fences: u32,
}
unsafe impl ByteValued for CmdSubmit {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceMapBlob {
    pub resource_id: u32,
    pub padding: u32,
    pub offset: u64,
}
unsafe impl ByteValued for ResourceMapBlob {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RespMapInfo {
    pub map_info: u32,
    pub padding: u32,
}
unsafe impl ByteValued for RespMapInfo {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CursorPos {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub padding: u32,
}
unsafe impl ByteValued for CursorPos {}

/// Bytes per pixel and memory layout of a 2D format.
pub fn format_layout(fmt: u32) -> Option<(crate::display::PixelLayout, bool)> {
    use crate::display::PixelLayout::*;
    Some(match fmt {
        format::B8G8R8A8_UNORM => (Bgra, false),
        format::B8G8R8X8_UNORM => (Bgra, true),
        format::A8R8G8B8_UNORM => (Argb, false),
        format::X8R8G8B8_UNORM => (Argb, true),
        format::R8G8B8A8_UNORM => (Rgba, false),
        format::R8G8B8X8_UNORM => (Rgba, true),
        format::A8B8G8R8_UNORM => (Abgr, false),
        format::X8B8G8R8_UNORM => (Abgr, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_spec() {
        assert_eq!(std::mem::size_of::<CtrlHdr>(), 24);
        assert_eq!(std::mem::size_of::<DisplayOne>(), 24);
        assert_eq!(std::mem::size_of::<TransferToHost2d>(), 32);
        assert_eq!(std::mem::size_of::<ResourceCreateBlob>(), 32);
        assert_eq!(std::mem::size_of::<SetScanoutBlob>(), 72);
        assert_eq!(std::mem::size_of::<CtxCreate>(), 72);
        assert_eq!(std::mem::size_of::<ResourceCreate3d>(), 48);
        assert_eq!(std::mem::size_of::<Transfer3d>(), 48);
        assert_eq!(std::mem::size_of::<ResourceMapBlob>(), 16);
    }
}
