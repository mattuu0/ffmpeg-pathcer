use std::ffi::c_void;

use windows::core::HRESULT;

use crate::hooks::dxgi_output::install_output_hooks;
use crate::hooks::slots;
use crate::hooks::vtable::{already_patched, patch_slot};
use crate::logging::plog;

type EnumOutputsFn =
    unsafe extern "system" fn(this: *mut c_void, output: u32, ppoutput: *mut *mut c_void) -> HRESULT;

static mut ORIGINAL_ENUM_OUTPUTS: Option<EnumOutputsFn> = None;

/// # Safety
/// `adapter_ptr` must be a live `IDXGIAdapter*` (or IDXGIAdapter1/2 -- they
/// all share the same EnumOutputs slot since COM interfaces only append,
/// never reorder, inherited slots).
pub unsafe fn install_adapter_hooks(adapter_ptr: *mut c_void) {
    let vtable_ptr = *(adapter_ptr as *mut *mut *mut c_void);
    if already_patched(vtable_ptr, slots::ENUM_OUTPUTS) {
        return;
    }

    let original = patch_slot(vtable_ptr, slots::ENUM_OUTPUTS, hooked_enum_outputs as *mut c_void);
    ORIGINAL_ENUM_OUTPUTS = Some(std::mem::transmute(original));
    plog!("hooked IDXGIAdapter::EnumOutputs");
}

/// Calls the REAL (un-hooked) `EnumOutputs`.
///
/// # Safety
/// Must only be called after `install_adapter_hooks` has run at least once.
pub unsafe fn call_original_enum_outputs(
    this: *mut c_void,
    output: u32,
    ppoutput: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_ENUM_OUTPUTS.expect("call_original_enum_outputs before hook install");
    original(this, output, ppoutput)
}

unsafe extern "system" fn hooked_enum_outputs(
    this: *mut c_void,
    output: u32,
    ppoutput: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_ENUM_OUTPUTS.expect("EnumOutputs hook fired before install");
    let hr = original(this, output, ppoutput);

    if hr.is_ok() && !ppoutput.is_null() && !(*ppoutput).is_null() {
        plog!("EnumOutputs succeeded (index {output}); wrapping DuplicateOutput");
        *crate::state::LAST_OUTPUT_INDEX.lock() = output;
        install_output_hooks(*ppoutput);
    }

    hr
}
