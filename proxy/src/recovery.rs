use windows::core::{Error, Interface, Result};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

use crate::logging::plog;
use crate::state::DuplicationSource;

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
/// A previous version of this module fell back to calling
/// `D3D11CreateDevice` fresh (`recreate_from_scratch`) whenever this
/// function failed repeatedly (e.g. while stuck on the secure desktop during
/// a UAC prompt). That fallback was removed entirely after real-world
/// UAC-transition testing: ddagrab itself keeps using the ORIGINAL
/// `ID3D11Device` it was handed at filter-init time for the rest of the
/// process's life and never learns about a replacement device created here,
/// so a frame acquired via a freshly-recreated device ends up being copied
/// through ddagrab's OWN (different) `ID3D11DeviceContext` --
/// `ID3D11DeviceContext_CopySubresourceRegion` across two distinct devices
/// silently fails, which ddagrab then surfaces as a fatal
/// `AVERROR_EXTERNAL`, killing capture outright. This function is now the
/// ONLY recovery path: it either succeeds by reusing ddagrab's own device,
/// or it keeps failing (and getting retried by the caller) until the
/// desktop switch resolves -- confirmed to resolve within about a second of
/// the input desktop returning to "Default".
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
    // ddagrab's own initial DuplicateOutput(1) call into two live real
    // duplication instances. Recovery itself now always runs inline on
    // ddagrab's own AcquireNextFrame-calling thread (no dedicated pump
    // thread), so there is no other concurrent recovery attempt to worry
    // about either -- this lock is kept for defense in depth regardless.
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
