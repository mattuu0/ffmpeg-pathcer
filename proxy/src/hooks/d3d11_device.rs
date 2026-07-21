use std::ffi::c_void;

use once_cell::sync::OnceCell;
use windows::core::{Interface, HRESULT, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::hooks::dxgi_device::install_device_hooks;
use crate::hooks::vtable::InlineHook;
use crate::logging::plog;

type D3D11CreateDeviceFn = unsafe extern "system" fn(
    padapter: *mut c_void,
    drivertype: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    pfeaturelevels: *const i32,
    featurelevels: u32,
    sdkversion: u32,
    ppdevice: *mut *mut c_void,
    pfeaturelevel: *mut i32,
    ppimmediatecontext: *mut *mut c_void,
) -> HRESULT;

static HOOK: OnceCell<InlineHook> = OnceCell::new();
static ORIGINAL: OnceCell<D3D11CreateDeviceFn> = OnceCell::new();

/// Installs an inline hook on `d3d11.dll!D3D11CreateDevice` so we observe
/// every `ID3D11Device` ffmpeg/avutil creates, regardless of whether avutil
/// resolved the address via static import or LoadLibrary+GetProcAddress.
///
/// # Safety
/// Must be called once, early (from DllMain), before any thread in the
/// process is likely to be calling D3D11CreateDevice concurrently.
pub unsafe fn install() {
    let _guard = crate::state::INSTALL_LOCK.lock();
    if HOOK.get().is_some() {
        return;
    }

    let module_name: Vec<u16> = "d3d11.dll\0".encode_utf16().collect();
    let module = match LoadLibraryW(PCWSTR(module_name.as_ptr())) {
        Ok(m) => m,
        Err(e) => {
            plog!("LoadLibraryW(d3d11.dll) failed: {e:?}");
            return;
        }
    };

    let proc_name = c"D3D11CreateDevice";
    let Some(proc) = GetProcAddress(module, windows::core::PCSTR(proc_name.as_ptr() as *const u8))
    else {
        plog!("GetProcAddress(D3D11CreateDevice) failed");
        return;
    };

    let original: D3D11CreateDeviceFn = std::mem::transmute(proc);
    let _ = ORIGINAL.set(original);

    let hook = InlineHook::install(proc as *mut c_void, hooked_d3d11_create_device as *mut c_void);
    let _ = HOOK.set(hook);

    plog!("installed D3D11CreateDevice inline hook");
}

unsafe extern "system" fn hooked_d3d11_create_device(
    padapter: *mut c_void,
    drivertype: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    pfeaturelevels: *const i32,
    featurelevels: u32,
    sdkversion: u32,
    ppdevice: *mut *mut c_void,
    pfeaturelevel: *mut i32,
    ppimmediatecontext: *mut *mut c_void,
) -> HRESULT {
    // IMPORTANT: `ORIGINAL` stores the *address* D3D11CreateDevice used to
    // live at, but that address's first 12 bytes are now our own jmp stub
    // (installed by InlineHook::install). Calling it directly would jump
    // straight back into this same hook, recursing until the stack overflows.
    // `call_through` restores the original bytes for the duration of the call.
    let original = *ORIGINAL.get().expect("hook fired before original was stored");
    let hook = HOOK.get().expect("hook fired before HOOK was stored");

    let hr = hook.call_through(|| {
        original(
            padapter,
            drivertype,
            software,
            flags,
            pfeaturelevels,
            featurelevels,
            sdkversion,
            ppdevice,
            pfeaturelevel,
            ppimmediatecontext,
        )
    });

    if hr.is_ok() && !ppdevice.is_null() && !(*ppdevice).is_null() {
        plog!("D3D11CreateDevice succeeded (hr={hr:?}); wrapping returned ID3D11Device");
        let device: ID3D11Device = std::mem::transmute_copy(&*ppdevice);
        // transmute_copy above does not bump the refcount; the raw pointer at
        // *ppdevice remains the single owning reference handed to the caller.
        // We only borrow `device` here to read its vtable pointer; forget it
        // immediately after so we never double-Release.
        install_device_hooks(&device);
        std::mem::forget(device);

        let mut guard = crate::state::DEVICE_CREATE_ARGS.lock();
        if guard.is_none() {
            let feature_levels = if pfeaturelevels.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(pfeaturelevels, featurelevels as usize).to_vec()
            };
            *guard = Some(crate::state::DeviceCreateArgs {
                adapter: padapter,
                driver_type: drivertype,
                software,
                flags,
                feature_levels,
                sdk_version: sdkversion,
            });
            plog!("stashed D3D11CreateDevice args for potential full re-init later");
        }
    }

    hr
}

/// Calls the REAL (un-hooked) `D3D11CreateDevice`, bypassing the inline hook
/// (which would otherwise recurse into `hooked_d3d11_create_device`). Used
/// when rebuilding a fresh device chain from scratch after a secure-desktop
/// transition.
///
/// # Safety
/// Must only be called after `install()` has run.
pub unsafe fn call_original_d3d11_create_device(
    padapter: *mut c_void,
    drivertype: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    pfeaturelevels: *const i32,
    featurelevels: u32,
    sdkversion: u32,
    ppdevice: *mut *mut c_void,
    pfeaturelevel: *mut i32,
    ppimmediatecontext: *mut *mut c_void,
) -> HRESULT {
    let original = *ORIGINAL.get().expect("call_original_d3d11_create_device before install()");
    let hook = HOOK.get().expect("call_original_d3d11_create_device before install()");
    hook.call_through(|| {
        original(
            padapter,
            drivertype,
            software,
            flags,
            pfeaturelevels,
            featurelevels,
            sdkversion,
            ppdevice,
            pfeaturelevel,
            ppimmediatecontext,
        )
    })
}

// Only used to keep IDXGIAdapter import referenced for callers of this module;
// avoids an unused-import warning when the adapter argument path is extended.
#[allow(dead_code)]
fn _type_check(_a: Option<IDXGIAdapter>) {}
