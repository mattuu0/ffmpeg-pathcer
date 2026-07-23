use std::ffi::c_void;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{
    Common::DXGI_FORMAT, IDXGIOutput1, IDXGIOutput5,
};

use crate::hooks::duplication_proxy::wrap_duplication;
use crate::hooks::slots;
use crate::hooks::vtable::{already_patched, patch_slot};
use crate::logging::plog;
use crate::state::DuplicationSource;

type QueryInterfaceFn =
    unsafe extern "system" fn(this: *mut c_void, iid: *const GUID, ppv: *mut *mut c_void) -> HRESULT;
type DuplicateOutputFn = unsafe extern "system" fn(
    this: *mut c_void,
    pdevice: *mut c_void,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT;
type DuplicateOutput1Fn = unsafe extern "system" fn(
    this: *mut c_void,
    pdevice: *mut c_void,
    flags: u32,
    supportedformatscount: u32,
    psupportedformats: *const DXGI_FORMAT,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT;

static mut ORIGINAL_OUTPUT_QI: Option<QueryInterfaceFn> = None;
static mut ORIGINAL_DUPLICATE_OUTPUT: Option<DuplicateOutputFn> = None;
static mut ORIGINAL_DUPLICATE_OUTPUT1: Option<DuplicateOutput1Fn> = None;

/// # Safety
/// `output_ptr` must be a live `IDXGIOutput*`.
pub unsafe fn install_output_hooks(output_ptr: *mut c_void) {
    let vtable_ptr = *(output_ptr as *mut *mut *mut c_void);
    if already_patched(vtable_ptr, slots::QUERY_INTERFACE) {
        return;
    }

    let original = patch_slot(vtable_ptr, slots::QUERY_INTERFACE, hooked_output_query_interface as *mut c_void);
    ORIGINAL_OUTPUT_QI = Some(std::mem::transmute(original));
    plog!("hooked IDXGIOutput::QueryInterface");
}

/// Calls the REAL (un-hooked) `QueryInterface` on an `IDXGIOutput`-family
/// vtable we've patched.
///
/// # Safety
/// Must only be called after `install_output_hooks` has run at least once.
pub unsafe fn call_original_output_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_OUTPUT_QI.expect("call_original_output_query_interface before hook install");
    original(this, iid, ppv)
}

unsafe extern "system" fn hooked_output_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_OUTPUT_QI.expect("Output QI hook fired before install");
    let hr = original(this, iid, ppv);

    if hr.is_ok() && !ppv.is_null() && !(*ppv).is_null() {
        let vtable_ptr = *(*ppv as *mut *mut *mut c_void);

        if *iid == IDXGIOutput1::IID && !already_patched(vtable_ptr, slots::DUPLICATE_OUTPUT) {
            plog!("QueryInterface -> IDXGIOutput1 observed; wrapping DuplicateOutput");
            let original_dup =
                patch_slot(vtable_ptr, slots::DUPLICATE_OUTPUT, hooked_duplicate_output as *mut c_void);
            ORIGINAL_DUPLICATE_OUTPUT = Some(std::mem::transmute(original_dup));
        } else if *iid == IDXGIOutput5::IID && !already_patched(vtable_ptr, slots::DUPLICATE_OUTPUT1) {
            plog!("QueryInterface -> IDXGIOutput5 observed; wrapping DuplicateOutput1");
            let original_dup1 = patch_slot(
                vtable_ptr,
                slots::DUPLICATE_OUTPUT1,
                hooked_duplicate_output1 as *mut c_void,
            );
            ORIGINAL_DUPLICATE_OUTPUT1 = Some(std::mem::transmute(original_dup1));
        }
    }

    hr
}

/// Calls the REAL (un-hooked) `DuplicateOutput`, bypassing our own vtable
/// patch. Used by `recovery::reacquire` to re-duplicate after ACCESS_LOST --
/// calling through the patched vtable would loop back into
/// `hooked_duplicate_output` and re-wrap the result in another
/// `DuplicationProxy` layered on top of the previous one, indefinitely.
///
/// # Safety
/// Must only be called after `install_output_hooks` + the IDXGIOutput1 QI
/// branch have already run at least once (i.e. after the first successful
/// DuplicateOutput), so `ORIGINAL_DUPLICATE_OUTPUT` is populated.
pub unsafe fn call_original_duplicate_output(
    this: *mut c_void,
    pdevice: *mut c_void,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_DUPLICATE_OUTPUT.expect("call_original_duplicate_output before any hook install");
    original(this, pdevice, ppoutputduplication)
}

/// Same as `call_original_duplicate_output` but for `DuplicateOutput1`.
///
/// # Safety
/// Same preconditions as `call_original_duplicate_output`.
pub unsafe fn call_original_duplicate_output1(
    this: *mut c_void,
    pdevice: *mut c_void,
    flags: u32,
    supportedformatscount: u32,
    psupportedformats: *const DXGI_FORMAT,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT {
    let original = ORIGINAL_DUPLICATE_OUTPUT1.expect("call_original_duplicate_output1 before any hook install");
    original(this, pdevice, flags, supportedformatscount, psupportedformats, ppoutputduplication)
}

unsafe extern "system" fn hooked_duplicate_output(
    this: *mut c_void,
    pdevice: *mut c_void,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT {
    // Attach THIS thread to the current input desktop (which may already be
    // the UAC secure desktop) BEFORE duplicating, not just after a later
    // AcquireNextFrame failure. Confirmed against a known-working reference
    // implementation (capture-helper): attaching once, before duplication,
    // is what makes the resulting IDXGIOutputDuplication instance itself
    // valid against a secure desktop -- attaching only reactively afterward
    // is not equivalent, because the instance was already bound to whatever
    // desktop was current at DuplicateOutput time.
    crate::recovery::attach_once_this_thread("DuplicateOutput");

    // See state::DUPLICATE_OUTPUT_LOCK: only one real DuplicateOutput/
    // DuplicateOutput1 call may be in flight process-wide at a time, so this
    // (ddagrab's own initial call) can never race a pump-thread recovery
    // attempt into producing two live real duplication instances at once.
    let _guard = crate::state::DUPLICATE_OUTPUT_LOCK.lock();
    let original = ORIGINAL_DUPLICATE_OUTPUT.expect("DuplicateOutput hook fired before install");
    let hr = original(this, pdevice, ppoutputduplication);

    if hr.is_ok() && !ppoutputduplication.is_null() && !(*ppoutputduplication).is_null() {
        plog!("DuplicateOutput succeeded; installing recovery-capable duplication proxy");
        let output1: IDXGIOutput1 = IDXGIOutput1::from_raw_borrowed(&this).unwrap().clone();
        let device: ID3D11Device = ID3D11Device::from_raw_borrowed(&pdevice).unwrap().clone();
        let source = DuplicationSource::V1 { output1, device };
        *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(source.clone());
        wrap_duplication(ppoutputduplication, source);
    }

    hr
}

unsafe extern "system" fn hooked_duplicate_output1(
    this: *mut c_void,
    pdevice: *mut c_void,
    flags: u32,
    supportedformatscount: u32,
    psupportedformats: *const DXGI_FORMAT,
    ppoutputduplication: *mut *mut c_void,
) -> HRESULT {
    crate::recovery::attach_once_this_thread("DuplicateOutput");

    // See state::DUPLICATE_OUTPUT_LOCK.
    let _guard = crate::state::DUPLICATE_OUTPUT_LOCK.lock();
    let original = ORIGINAL_DUPLICATE_OUTPUT1.expect("DuplicateOutput1 hook fired before install");
    let hr = original(this, pdevice, flags, supportedformatscount, psupportedformats, ppoutputduplication);

    if hr.is_ok() && !ppoutputduplication.is_null() && !(*ppoutputduplication).is_null() {
        plog!("DuplicateOutput1 succeeded; installing recovery-capable duplication proxy");
        let output5: IDXGIOutput5 = IDXGIOutput5::from_raw_borrowed(&this).unwrap().clone();
        let device: ID3D11Device = ID3D11Device::from_raw_borrowed(&pdevice).unwrap().clone();
        let supported_formats =
            std::slice::from_raw_parts(psupportedformats, supportedformatscount as usize).to_vec();
        let source = DuplicationSource::V5 { output5, device, flags, supported_formats };
        *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(source.clone());
        wrap_duplication(ppoutputduplication, source);
    }

    hr
}
