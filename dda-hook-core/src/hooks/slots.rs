//! Vtable slot indices, computed from the `windows` crate's own `_Vtbl`
//! struct layouts via `offset_of!` rather than hand-counted, so a future
//! `windows` crate bump that changed a struct's field order (it won't --
//! shipped COM interfaces are ABI-frozen -- but just in case) would fail to
//! compile instead of silently hooking the wrong slot.

use std::mem::{offset_of, size_of};

use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter_Vtbl, IDXGIDevice_Vtbl, IDXGIObject_Vtbl, IDXGIOutput1_Vtbl, IDXGIOutput5_Vtbl,
    IDXGIOutputDuplication_Vtbl,
};

const PTR_SIZE: usize = size_of::<*mut core::ffi::c_void>();

pub const GET_ADAPTER: usize = offset_of!(IDXGIDevice_Vtbl, GetAdapter) / PTR_SIZE;
/// `IDXGIDevice::GetParent` -- what ddagrab actually calls (with riid =
/// IID_IDXGIAdapter) to get its adapter, NOT `GetAdapter`. `GetParent` is
/// inherited from `IDXGIObject`, so its offset is taken from that base vtable.
pub const GET_PARENT: usize = offset_of!(IDXGIObject_Vtbl, GetParent) / PTR_SIZE;
pub const ENUM_OUTPUTS: usize = offset_of!(IDXGIAdapter_Vtbl, EnumOutputs) / PTR_SIZE;
pub const DUPLICATE_OUTPUT: usize = offset_of!(IDXGIOutput1_Vtbl, DuplicateOutput) / PTR_SIZE;
pub const DUPLICATE_OUTPUT1: usize = offset_of!(IDXGIOutput5_Vtbl, DuplicateOutput1) / PTR_SIZE;
pub const ACQUIRE_NEXT_FRAME: usize =
    offset_of!(IDXGIOutputDuplication_Vtbl, AcquireNextFrame) / PTR_SIZE;
pub const RELEASE_FRAME: usize = offset_of!(IDXGIOutputDuplication_Vtbl, ReleaseFrame) / PTR_SIZE;

pub const QUERY_INTERFACE: usize = 0;
