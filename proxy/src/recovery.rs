use windows::core::{Error, Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

use crate::hooks::d3d11_device::call_original_d3d11_create_device;
use crate::hooks::dxgi_adapter::call_original_enum_outputs;
use crate::hooks::dxgi_device::{call_original_get_parent, call_original_query_interface};
use crate::hooks::dxgi_output::call_original_output_query_interface;
use crate::logging::plog;
use crate::state::{DeviceCreateArgs, DuplicationSource};

std::thread_local! {
    static ATTACHED_THIS_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Attaches the CALLING thread to the current input desktop (which may
/// already be the UAC secure desktop), exactly once per thread.
///
/// SetThreadDesktop only affects the thread that calls it. ddagrab calls
/// DuplicateOutput/DuplicateOutput1 once during init (typically the main/
/// filtergraph-setup thread) but then polls AcquireNextFrame from a
/// *different* worker thread for the lifetime of the capture -- confirmed
/// via thread-id logging. Attaching only the init thread therefore does
/// nothing for the thread that actually pumps frames, so this is called
/// from both call sites (dxgi_output.rs before the first DuplicateOutput,
/// and duplication_proxy.rs before the first AcquireNextFrame on whatever
/// thread ddagrab polls from) -- each thread attaches itself the first time
/// it's about to touch either call.
pub fn attach_once_this_thread(call_site: &str) {
    let already = ATTACHED_THIS_THREAD.with(|c| c.get());
    if already {
        return;
    }
    ATTACHED_THIS_THREAD.with(|c| c.set(true));

    match attach_input_desktop() {
        Ok(name) => plog!("[{call_site}] attached this thread to input desktop (name={name:?})"),
        Err(e) => plog!("[{call_site}] attach_input_desktop failed (continuing anyway): {e:?}"),
    }
}

/// Result of a rebuild: a fresh, real (unwrapped) duplication instance, plus
/// a fresh `DuplicationSource` describing how it was produced, so the next
/// recovery attempt rebuilds from THIS instance's lineage rather than the
/// original (possibly now-stale) one.
pub struct Rebuilt {
    pub duplication: IDXGIOutputDuplication,
    pub source: DuplicationSource,
}

/// Re-attaches this thread to the current input desktop, then re-issues
/// DuplicateOutput/DuplicateOutput1 on the SAME long-lived `ID3D11Device` +
/// `IDXGIOutput` that produced `source` -- WITHOUT creating a new device.
///
/// This mirrors the recovery strategy of the `win_desktop_duplication` crate
/// (confirmed by reading its `reacquire_dup`, in `duplication.rs`): on
/// ACCESS_LOST it drops only the dead `IDXGIOutputDuplication` and calls
/// `DuplicateOutput1` again on the very same device/output, never
/// re-creating the device. That crate's approach was cross-checked against
/// a minimal standalone repro (`desktop-shot`) which recovers reliably
/// across repeated Default<->Winlogon transitions using exactly this
/// device-reuse strategy -- including capturing secure-desktop frames.
///
/// A previous version of this module instead called `D3D11CreateDevice`
/// fresh on every recovery (`recreate_from_scratch`, still below). That
/// repeatedly produced a duplication instance whose very first
/// AcquireNextFrame itself failed with ACCESS_LOST, over and over, and the
/// failure persisted even long after the desktop had returned to Default --
/// consistent with repeated full device re-creation exhausting or wedging
/// some GPU/driver-side duplication registration that `desktop-shot` (which
/// never re-creates the device) does not hit. So: try the lightweight,
/// device-reusing path first, and only fall back to a full from-scratch
/// rebuild if this device/output pairing itself has gone bad (e.g. after a
/// display-mode change server-side, which DOES require a new device).
pub fn reduplicate_same_device() -> Result<Rebuilt> {
    let name = attach_input_desktop().unwrap_or_else(|e| {
        plog!("attach_input_desktop before reduplicate failed (continuing anyway): {e:?}");
        String::new()
    });
    plog!("input desktop is {name:?}; reduplicating on the existing device/output");

    let source = crate::state::LAST_DUPLICATION_SOURCE
        .lock()
        .clone()
        .ok_or_else(|| Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    // See state::DUPLICATE_OUTPUT_LOCK: guarantees this can never overlap
    // ddagrab's own initial DuplicateOutput(1) call (or, in principle,
    // another recovery attempt, though only the single pump thread ever
    // drives recovery today) into two live real duplication instances.
    let _guard = crate::state::DUPLICATE_OUTPUT_LOCK.lock();

    match source {
        DuplicationSource::V5 { output5, device, flags, supported_formats } => {
            let dup = duplicate_output1(&output5, &device, flags, &supported_formats)?;
            plog!("reduplicate_same_device: succeeded via DuplicateOutput1 (same device)");
            Ok(Rebuilt { duplication: dup, source: DuplicationSource::V5 { output5, device, flags, supported_formats } })
        }
        DuplicationSource::V1 { output1, device } => {
            let dup = duplicate_output(&output1, &device)?;
            plog!("reduplicate_same_device: succeeded via DuplicateOutput (same device)");
            Ok(Rebuilt { duplication: dup, source: DuplicationSource::V1 { output1, device } })
        }
    }
}

/// Rebuilds the ENTIRE device -> IDXGIDevice -> IDXGIAdapter -> IDXGIOutput ->
/// IDXGIOutputDuplication chain from scratch, starting from a brand new
/// `D3D11CreateDevice` call, rather than reusing the previous chain's
/// objects. Only used as a fallback when `reduplicate_same_device` itself
/// fails (e.g. DXGI_ERROR_UNSUPPORTED after the device/output pairing itself
/// has gone bad), since full re-creation was observed to be actively harmful
/// as the FIRST recovery attempt (see `reduplicate_same_device` doc comment).
pub fn recreate_from_scratch() -> Result<Rebuilt> {
    let name = attach_input_desktop().unwrap_or_else(|e| {
        plog!("attach_input_desktop before full rebuild failed (continuing anyway): {e:?}");
        String::new()
    });

    // A prior version of this function skipped rebuilding while the input
    // desktop was secure (Winlogon during a UAC prompt/lock screen), based
    // on the `scrap` crate's recovery pattern (discard-and-recreate only
    // after returning to normal). That was cross-checked against a minimal,
    // standalone repro (`desktop-shot`, using the same win_desktop_duplication
    // + attach-then-duplicate approach as this module) and disproven: with
    // this thread re-attached to the input desktop BEFORE each rebuild
    // (which the earlier attempts were also doing here), rebuilding WHILE on
    // Winlogon succeeds immediately and produces a working duplication
    // instance that captures the secure desktop itself (confirmed via saved
    // frames showing the UAC consent dialog). The ~100 failed rebuild
    // attempts observed earlier were not caused by rebuilding on a secure
    // desktop per se; skipping the rebuild only meant capture stayed frozen
    // for the entire UAC prompt and, in the reported regression, sometimes
    // did not resume afterward either. So: always attempt the rebuild,
    // regardless of which desktop is current.
    plog!("input desktop is {name:?}; attempting full rebuild");

    let args = crate::state::DEVICE_CREATE_ARGS
        .lock()
        .clone()
        .ok_or_else(|| Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    // See state::DUPLICATE_OUTPUT_LOCK. Held across the whole rebuild, not
    // just the final DuplicateOutput(1) call, since D3D11CreateDevice here
    // and ddagrab's own initial call both ultimately feed into the same
    // "how many live real duplication instances exist" invariant.
    let _guard = crate::state::DUPLICATE_OUTPUT_LOCK.lock();

    let device = create_device(&args)?;
    let dxgi_device = query_interface::<IDXGIDevice>(Interface::as_raw(&device), call_original_query_interface)?;
    let adapter = get_parent_adapter(&dxgi_device)?;
    let output = enum_output(&adapter, 0)?;

    // Prefer IDXGIOutput5::DuplicateOutput1 (matches ddagrab's own preference
    // when available), falling back to IDXGIOutput1::DuplicateOutput.
    if let Ok(output5) = query_interface::<IDXGIOutput5>(Interface::as_raw(&output), call_original_output_query_interface) {
        let supported_formats = vec![windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM];
        let dup = duplicate_output1(&output5, &device, 0, &supported_formats)?;
        plog!("recreate_from_scratch: full rebuild succeeded via DuplicateOutput1");
        return Ok(Rebuilt {
            duplication: dup,
            source: DuplicationSource::V5 { output5, device, flags: 0, supported_formats },
        });
    }

    let output1 = query_interface::<IDXGIOutput1>(Interface::as_raw(&output), call_original_output_query_interface)?;
    let dup = duplicate_output(&output1, &device)?;
    plog!("recreate_from_scratch: full rebuild succeeded via DuplicateOutput");
    Ok(Rebuilt { duplication: dup, source: DuplicationSource::V1 { output1, device } })
}

fn create_device(args: &DeviceCreateArgs) -> Result<ID3D11Device> {
    unsafe {
        let mut raw_device: *mut core::ffi::c_void = std::ptr::null_mut();
        let feature_levels_ptr =
            if args.feature_levels.is_empty() { std::ptr::null() } else { args.feature_levels.as_ptr() };

        let hr = call_original_d3d11_create_device(
            args.adapter,
            args.driver_type,
            args.software,
            args.flags,
            feature_levels_ptr,
            args.feature_levels.len() as u32,
            args.sdk_version,
            &mut raw_device,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        hr.ok()?;
        Ok(Interface::from_raw(raw_device))
    }
}

fn query_interface<T: Interface>(
    this: *mut core::ffi::c_void,
    call: unsafe fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> windows::core::HRESULT,
) -> Result<T> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        call(this, &T::IID, &mut raw).ok()?;
        Ok(Interface::from_raw(raw))
    }
}

fn get_parent_adapter(device: &IDXGIDevice) -> Result<IDXGIAdapter> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        call_original_get_parent(Interface::as_raw(device), &IDXGIAdapter::IID, &mut raw).ok()?;
        Ok(Interface::from_raw(raw))
    }
}

fn enum_output(adapter: &IDXGIAdapter, index: u32) -> Result<windows::Win32::Graphics::Dxgi::IDXGIOutput> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        call_original_enum_outputs(Interface::as_raw(adapter), index, &mut raw).ok()?;
        Ok(Interface::from_raw(raw))
    }
}

fn duplicate_output(output1: &IDXGIOutput1, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        crate::hooks::dxgi_output::call_original_duplicate_output(
            Interface::as_raw(output1),
            Interface::as_raw(device),
            &mut raw,
        )
        .ok()?;
        Ok(Interface::from_raw(raw))
    }
}

fn duplicate_output1(
    output5: &IDXGIOutput5,
    device: &ID3D11Device,
    flags: u32,
    supported_formats: &[windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT],
) -> Result<IDXGIOutputDuplication> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        crate::hooks::dxgi_output::call_original_duplicate_output1(
            Interface::as_raw(output5),
            Interface::as_raw(device),
            flags,
            supported_formats.len() as u32,
            supported_formats.as_ptr(),
            &mut raw,
        )
        .ok()?;
        Ok(Interface::from_raw(raw))
    }
}

fn attach_input_desktop() -> Result<String> {
    unsafe {
        let tid = GetCurrentThreadId();
        let access = DESKTOP_ACCESS_FLAGS(0x01FF); // GENERIC_ALL-equivalent for desktop objects
        let desktop = OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, false, access)?;

        let name = desktop_name(desktop);
        plog!("[tid={tid}] OpenInputDesktop succeeded, name={name:?}");

        let switch_result = SetThreadDesktop(desktop);
        let _ = CloseDesktop(desktop);
        switch_result?;
        plog!("[tid={tid}] SetThreadDesktop succeeded");

        // Verify what desktop this thread is ACTUALLY attached to now,
        // rather than trusting SetThreadDesktop's success return alone --
        // if this doesn't match `name`, something is silently not taking
        // effect (e.g. a security restriction that fails open rather than
        // returning an error).
        if let Ok(current) = windows::Win32::System::StationsAndDesktops::GetThreadDesktop(tid) {
            let current_name = desktop_name(current);
            plog!("[tid={tid}] GetThreadDesktop verification: currently attached to {current_name:?}");
        }

        Ok(name)
    }
}

unsafe fn desktop_name(desktop: windows::Win32::System::StationsAndDesktops::HDESK) -> String {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    match GetUserObjectInformationW(
        windows::Win32::Foundation::HANDLE(desktop.0),
        UOI_NAME,
        Some(buf.as_mut_ptr() as *mut _),
        (buf.len() * 2) as u32,
        Some(&mut needed),
    ) {
        Ok(()) => {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        }
        Err(e) => format!("<unknown: {e:?}>"),
    }
}
