use std::ffi::c_void;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};

use crate::hooks::dxgi_adapter::install_adapter_hooks;
use crate::hooks::slots;
use crate::hooks::vtable::{already_patched, patch_slot};
use crate::logging::plog;

type QueryInterfaceFn =
    unsafe extern "system" fn(this: *mut c_void, iid: *const GUID, ppv: *mut *mut c_void) -> HRESULT;
type GetParentFn = unsafe extern "system" fn(
    this: *mut c_void,
    riid: *const GUID,
    ppparent: *mut *mut c_void,
) -> HRESULT;

static mut ORIGINAL_DEVICE_QI: Option<QueryInterfaceFn> = None;
static mut ORIGINAL_GET_PARENT: Option<GetParentFn> = None;

/// Hooks `QueryInterface` on the vtable shared by `device` so that when
/// ddagrab/avutil QIs for `IDXGIDevice`/`IDXGIDevice1`/`IDXGIDevice2`, we get a
/// chance to hook `GetParent` (inherited from `IDXGIObject`) on the (also
/// class-shared) IDXGIDevice vtable before returning it.
///
/// ddagrab obtains its adapter via `IDXGIDevice::GetParent(&IID_IDXGIAdapter,
/// ...)`, NOT via `IDXGIDevice::GetAdapter` -- confirmed against
/// `libavfilter/vsrc_ddagrab.c`'s `init_dxgi_dda`. `GetAdapter` exists on the
/// interface but ddagrab never calls it; hooking it alone is a no-op.
pub unsafe fn install_device_hooks(device: &ID3D11Device) {
    let vtable_ptr = *(device.as_raw() as *mut *mut *mut c_void);
    if already_patched(vtable_ptr, slots::QUERY_INTERFACE) {
        return;
    }

    let original = patch_slot(vtable_ptr, slots::QUERY_INTERFACE, hooked_device_query_interface as *mut c_void);
    ORIGINAL_DEVICE_QI = Some(std::mem::transmute(original));
    plog!("hooked ID3D11Device::QueryInterface");
}

/// Calls the REAL (un-hooked) `QueryInterface` on an `ID3D11Device`/
/// `IDXGIDevice`-family vtable we've patched. Used when rebuilding a fresh
/// device->adapter->output->duplication chain from scratch so we don't loop
/// back into our own hooks and re-wrap things.
///
/// # Safety
/// Must only be called after `install_device_hooks` has run at least once.
pub unsafe fn call_original_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_DEVICE_QI.expect("call_original_query_interface before hook install");
    original(this, iid, ppv)
}

/// Calls the REAL (un-hooked) `GetParent`.
///
/// # Safety
/// Must only be called after the `IID_IDXGIDevice` QI branch has run at
/// least once (i.e. after `hooked_device_query_interface` observed one).
pub unsafe fn call_original_get_parent(
    this: *mut c_void,
    riid: *const GUID,
    ppparent: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_GET_PARENT.expect("call_original_get_parent before hook install");
    original(this, riid, ppparent)
}

unsafe extern "system" fn hooked_device_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_DEVICE_QI.expect("QI hook fired before install");
    let hr = original(this, iid, ppv);

    if hr.is_ok() && !ppv.is_null() && !(*ppv).is_null() && *iid == IDXGIDevice::IID {
        plog!("QueryInterface -> IDXGIDevice observed; wrapping GetParent");
        let vtable_ptr = *(*ppv as *mut *mut *mut c_void);
        if !already_patched(vtable_ptr, slots::GET_PARENT) {
            let original_gp = patch_slot(vtable_ptr, slots::GET_PARENT, hooked_get_parent as *mut c_void);
            ORIGINAL_GET_PARENT = Some(std::mem::transmute(original_gp));
            plog!("hooked IDXGIDevice::GetParent");
        }
    }

    hr
}

unsafe extern "system" fn hooked_get_parent(
    this: *mut c_void,
    riid: *const GUID,
    ppparent: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_GET_PARENT.expect("GetParent hook fired before install");
    let hr = original(this, riid, ppparent);

    if hr.is_ok()
        && !ppparent.is_null()
        && !(*ppparent).is_null()
        && !riid.is_null()
        && *riid == IDXGIAdapter::IID
    {
        plog!("GetParent(IID_IDXGIAdapter) succeeded; wrapping EnumOutputs");
        install_adapter_hooks(*ppparent);
    }

    hr
}
