use windows::core::{Error, Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

use crate::hooks::dxgi_adapter::call_original_enum_outputs;
use crate::hooks::dxgi_device::{call_original_get_parent, call_original_query_interface};
use crate::hooks::dxgi_output::call_original_output_query_interface;
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

/// Re-attaches this thread to the current input desktop, re-enumerates the
/// output at `LAST_OUTPUT_INDEX` fresh (via `IDXGIAdapter::EnumOutputs`) from
/// the SAME long-lived `ID3D11Device` that produced the last known-good
/// duplication, and re-duplicates from THAT freshly enumerated `IDXGIOutput`
/// -- WITHOUT creating a new device.
///
/// An earlier version of this function instead reused a cached `IDXGIOutput`
/// object directly (no re-enumeration) -- cheaper, and it mirrored the
/// recovery strategy of the `win_desktop_duplication` crate (confirmed by
/// reading its `reacquire_dup`, in `duplication.rs`), cross-checked against
/// a minimal standalone repro (`desktop-shot`) that recovers reliably across
/// repeated Default<->Winlogon transitions on a normal physical monitor.
/// That was proven insufficient by a real crash report, though: Windows PnP
/// event logs (`Microsoft-Windows-Kernel-PnP/Configuration`) confirmed that
/// virtual display adapters (Parsec's Virtual Display Adapter) tear down and
/// recreate their display device with a NEW PnP instance ID (e.g.
/// `DISPLAY\PSCCDD0\...&UID256` changing suffix from `&8&` to `&9&`) across
/// a rapid Winlogon<->Default switch -- something a REAL physical monitor
/// never does. Against a torn-down display, `DuplicateOutput1` on the STALE
/// cached `IDXGIOutput` could still return success, but the resulting
/// duplication session wasn't actually backed by a live display: the
/// subsequent `ReleaseFrame` call failed (ddagrab surfaced this as a fatal
/// `AVERROR_EXTERNAL`, "DDA ReleaseFrame failed!"), well after recovery had
/// already reported itself successful. Since a stale-but-still-"successful"
/// DuplicateOutput1 call can't be distinguished from a healthy one at the
/// point recovery runs, the fix is to never trust a cached output at all:
/// always re-enumerate fresh, every recovery.
///
/// A previous version of this module also had a `recreate_from_scratch`
/// fallback that re-created the `ID3D11Device` ITSELF (not just the output)
/// whenever this function failed repeatedly (e.g. while stuck on the secure
/// desktop during a UAC prompt). That was removed entirely after real-world
/// UAC-transition testing: ddagrab itself keeps using the ORIGINAL
/// `ID3D11Device` it was handed at filter-init time for the rest of the
/// process's life and never learns about a replacement device created here,
/// so a frame acquired via a freshly-recreated device ends up being copied
/// through ddagrab's OWN (different) `ID3D11DeviceContext` --
/// `ID3D11DeviceContext_CopySubresourceRegion` across two distinct devices
/// silently fails, which ddagrab then surfaces as a fatal
/// `AVERROR_EXTERNAL`, killing capture outright. This function is now the
/// ONLY recovery path: it either succeeds by reusing ddagrab's own device
/// (just re-enumerating the output on it), or it keeps failing (and getting
/// retried by the caller) until the desktop switch resolves.
pub fn renumerate_output_and_duplicate() -> Result<Rebuilt> {
    let name = attach_input_desktop().unwrap_or_else(|e| {
        plog!("attach_input_desktop before output re-enumeration failed (continuing anyway): {e:?}");
        String::new()
    });
    plog!("input desktop is {name:?}; re-enumerating output on the existing device");

    let device = crate::state::LAST_DUPLICATION_SOURCE
        .lock()
        .clone()
        .map(|source| match source {
            DuplicationSource::V5 { device, .. } => device,
            DuplicationSource::V1 { device, .. } => device,
        })
        .ok_or_else(|| Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    let output_idx = *crate::state::LAST_OUTPUT_INDEX.lock();

    // See state::DUPLICATE_OUTPUT_LOCK.
    let _guard = crate::state::DUPLICATE_OUTPUT_LOCK.lock();

    let dxgi_device = query_interface::<IDXGIDevice>(Interface::as_raw(&device), call_original_query_interface)?;
    let adapter = get_parent_adapter(&dxgi_device)?;
    let output = enum_output(&adapter, output_idx)?;

    if let Ok(output5) = query_interface::<IDXGIOutput5>(Interface::as_raw(&output), call_original_output_query_interface) {
        let supported_formats = vec![windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM];
        let dup = duplicate_output1(&output5, &device, 0, &supported_formats)?;
        plog!("renumerate_output_and_duplicate: succeeded via DuplicateOutput1 (re-enumerated output, same device)");
        return Ok(Rebuilt {
            duplication: dup,
            source: DuplicationSource::V5 { output5, device, flags: 0, supported_formats },
        });
    }

    let output1 = query_interface::<IDXGIOutput1>(Interface::as_raw(&output), call_original_output_query_interface)?;
    let dup = duplicate_output(&output1, &device)?;
    plog!("renumerate_output_and_duplicate: succeeded via DuplicateOutput (re-enumerated output, same device)");
    Ok(Rebuilt { duplication: dup, source: DuplicationSource::V1 { output1, device } })
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

/// Gets the parent `IDXGIAdapter` for `device`, first checking whether the
/// DXGI adapter/output topology has become stale (`IDXGIFactory1::IsCurrent`
/// returning `false`) and, if so, re-fetching the SAME physical adapter (by
/// `AdapterLuid`, which identifies a specific GPU and is stable across
/// topology changes) from a freshly created `IDXGIFactory1` instead.
///
/// Why this check is necessary: confirmed via Windows PnP event logs that a
/// virtual display adapter (Parsec's Virtual Display Adapter) tears down
/// and recreates its display device (new PnP instance ID) across a rapid
/// Winlogon<->Default desktop switch. Simply re-enumerating outputs on the
/// SAME (possibly now-stale) `IDXGIAdapter` object was observed to still
/// return a "successful" `DuplicateOutput1` against a duplication session
/// that wasn't actually backed by a live display (its `ReleaseFrame`
/// subsequently failed) -- i.e. the staleness lives one level up, on the
/// adapter/factory itself, not just the specific `IDXGIOutput` object.
/// `IsCurrent()` is DXGI's own purpose-built signal for exactly this
/// situation (its documented use case is "an adapter was added or removed").
fn get_parent_adapter(device: &IDXGIDevice) -> Result<IDXGIAdapter> {
    let adapter: IDXGIAdapter = unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        call_original_get_parent(Interface::as_raw(device), &IDXGIAdapter::IID, &mut raw).ok()?;
        Interface::from_raw(raw)
    };

    let factory: IDXGIFactory1 = unsafe { adapter.GetParent()? };
    if unsafe { factory.IsCurrent() }.as_bool() {
        return Ok(adapter);
    }

    plog!("get_parent_adapter: DXGI topology is stale (IsCurrent()==false); re-creating factory/adapter");
    let target_luid = unsafe { adapter.GetDesc()?.AdapterLuid };

    let fresh_factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut index = 0u32;
    loop {
        let candidate: IDXGIAdapter = match unsafe { fresh_factory.EnumAdapters(index) } {
            Ok(a) => a,
            Err(e) => return Err(e),
        };
        let desc = unsafe { candidate.GetDesc()? };
        if desc.AdapterLuid.LowPart == target_luid.LowPart && desc.AdapterLuid.HighPart == target_luid.HighPart {
            plog!("get_parent_adapter: re-acquired the same physical adapter (luid match) from a fresh factory");
            return Ok(candidate);
        }
        index += 1;
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
