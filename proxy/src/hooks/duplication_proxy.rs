use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{implement, IUnknownImpl, Interface, OutRef, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    IDXGIObject_Impl, IDXGIOutputDuplication, IDXGIOutputDuplication_Impl, IDXGIResource,
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAPPED_RECT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

use crate::logging::plog;
use crate::state::DuplicationSource;

/// A COM object implementing `IDXGIOutputDuplication` that ddagrab holds and
/// calls. Unlike an earlier version of this module, THIS ONE calls straight
/// through to the real `IDXGIOutputDuplication` on whatever thread ddagrab
/// itself calls from -- there is no dedicated background thread polling the
/// real instance independently. Recovery on ACCESS_LOST/ACCESS_DENIED/
/// INVALID_CALL happens synchronously, inline, on that same call: drop the
/// dead instance, re-duplicate, and retry once, all before returning to
/// ddagrab.
///
/// Why the earlier "hijack + dedicated pump thread" design was replaced:
/// confirmed via [stats/1s] diagnostics that ddagrab's own AcquireNextFrame
/// calls (relayed through the old DuplicationProxy) landed cleanly nearly
/// every time (hits == calls, timeouts == 0) -- the pump thread's polling
/// itself was never the bottleneck. But ddagrab's own frame REQUEST rate
/// (ddagrab_request_frame in vsrc_ddagrab.c) dropped from 60Hz to as low as
/// 1-2Hz over time whenever an encoder (hevc_nvenc) was downstream, and
/// persisted even after cutting the pump's per-frame GPU copy from 2 down to
/// 1 (a ring-buffer redesign that eliminated the extra CopyResource but left
/// throughput unchanged). That pointed at the pump thread's mere existence
/// (a second thread continuously touching the same ID3D11Device/
/// ImmediateContext ddagrab and NVENC also share) as the actual interference
/// source, not anything it was doing per frame. Removing the thread
/// entirely -- ddagrab's own polling thread now IS the only thing that ever
/// touches the real duplication instance -- eliminates that source outright.
///
/// This does reintroduce the original problem a dedicated pump thread was
/// built to solve: ddagrab (libavfilter/vsrc_ddagrab.c, next_frame_internal)
/// treats ANY AcquireNextFrame failure other than DXGI_ERROR_WAIT_TIMEOUT as
/// fatal, with zero recovery of its own. So recovery here must be complete
/// -- the dead instance dropped and a replacement successfully re-duplicated
/// -- BEFORE this call is allowed to return anything other than Ok(()) or
/// WAIT_TIMEOUT to ddagrab; ACCESS_LOST/ACCESS_DENIED/INVALID_CALL must
/// never be the HRESULT ddagrab itself observes. See `recovery.rs`'s module
/// doc comment for why "drop the dead instance before requesting its
/// replacement" is itself required for the re-duplicate call to succeed at
/// all (a DXGI-level constraint, unrelated to threading).
#[implement(IDXGIOutputDuplication)]
pub struct DuplicationProxy {
    /// The real instance, or `None` if the last recovery attempt itself
    /// failed (rare -- only when both reduplicate_same_device AND
    /// recreate_from_scratch fail, e.g. mid display-mode-change). `None`
    /// means every AcquireNextFrame call returns WAIT_TIMEOUT until a later
    /// call's own recovery attempt succeeds.
    real: Mutex<Option<IDXGIOutputDuplication>>,
    source: Mutex<DuplicationSource>,
    desc: DXGI_OUTDUPL_DESC,
    // DIAGNOSTIC: tallies of what ddagrab's own AcquireNextFrame calls
    // actually see, kept from the previous design to compare thread-free
    // throughput against the old pump-thread numbers.
    stats_calls: AtomicU64,
    stats_hits: AtomicU64,
    stats_timeouts: AtomicU64,
    stats_recoveries: AtomicU64,
    stats_recovery_failures: AtomicU64,
    stats_last_timeout_ms: AtomicU64,
    stats_window_start: Mutex<Instant>,
}

/// Replaces `*ppoutputduplication` (a real `IDXGIOutputDuplication*` freshly
/// returned by `DuplicateOutput`/`DuplicateOutput1`) with a `DuplicationProxy`
/// that owns it directly -- no background thread involved.
///
/// # Safety
/// `ppoutputduplication` must point at a valid, just-returned
/// `IDXGIOutputDuplication*` matching a successful HRESULT.
pub unsafe fn wrap_duplication(ppoutputduplication: *mut *mut c_void, source: DuplicationSource) {
    let raw = *ppoutputduplication;
    // Takes over the single owning ref the real DuplicateOutput call already
    // produced at `raw` -- no AddRef needed.
    let real: IDXGIOutputDuplication = Interface::from_raw(raw);
    let desc = unsafe { real.GetDesc() };

    *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(source.clone());

    let proxy = DuplicationProxy {
        real: Mutex::new(Some(real)),
        source: Mutex::new(source),
        desc,
        stats_calls: AtomicU64::new(0),
        stats_hits: AtomicU64::new(0),
        stats_timeouts: AtomicU64::new(0),
        stats_recoveries: AtomicU64::new(0),
        stats_recovery_failures: AtomicU64::new(0),
        stats_last_timeout_ms: AtomicU64::new(0),
        stats_window_start: Mutex::new(Instant::now()),
    };
    let com_proxy: IDXGIOutputDuplication = proxy.into();

    // Hand out our wrapper in place of the real pointer; the caller (ddagrab)
    // never sees the real object's address.
    *ppoutputduplication = com_proxy.into_raw();
}

impl IDXGIObject_Impl for DuplicationProxy_Impl {
    // ddagrab never calls IDXGIObject methods on the duplication instance
    // (confirmed from source) -- stub these out rather than routing an
    // unused path through to the real instance.
    fn SetPrivateData(&self, _name: *const windows::core::GUID, _datasize: u32, _pdata: *const c_void) -> Result<()> {
        Ok(())
    }

    fn SetPrivateDataInterface(&self, _name: *const windows::core::GUID, _punknown: windows::core::Ref<'_, windows::core::IUnknown>) -> Result<()> {
        Ok(())
    }

    fn GetPrivateData(&self, _name: *const windows::core::GUID, _pdatasize: *mut u32, _pdata: *mut c_void) -> Result<()> {
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn GetParent(&self, _riid: *const windows::core::GUID, _ppparent: *mut *mut c_void) -> Result<()> {
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }
}

impl IDXGIOutputDuplication_Impl for DuplicationProxy_Impl {
    fn GetDesc(&self, pdesc: *mut DXGI_OUTDUPL_DESC) {
        unsafe { *pdesc = self.desc };
    }

    fn AcquireNextFrame(
        &self,
        timeoutinmilliseconds: u32,
        pframeinfo: *mut DXGI_OUTDUPL_FRAME_INFO,
        ppdesktopresource: OutRef<'_, IDXGIResource>,
    ) -> Result<()> {
        crate::recovery::attach_once_this_thread("AcquireNextFrame (passthrough)");

        self.stats_calls.fetch_add(1, Ordering::Relaxed);
        self.stats_last_timeout_ms.store(timeoutinmilliseconds as u64, Ordering::Relaxed);

        let mut real_guard = self.real.lock().unwrap();

        // If the last call's recovery attempt itself failed, `real_guard` is
        // `None` here -- retry recovery on THIS call too, rather than just
        // reporting WAIT_TIMEOUT and giving up until some other call
        // happens to retry. Without this retry, a single transient recovery
        // failure (e.g. attach_input_desktop racing the desktop switch
        // itself) would strand the filter in "no frame, ever" permanently,
        // even though recovery would very likely succeed on the very next
        // attempt a few milliseconds later.
        if real_guard.is_none() {
            match try_recover() {
                Ok(rebuilt) => {
                    self.stats_recoveries.fetch_add(1, Ordering::Relaxed);
                    plog!("[DuplicationProxy::AcquireNextFrame] recovered inline (retry of a previously failed recovery)");
                    *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(rebuilt.source.clone());
                    *self.source.lock().unwrap() = rebuilt.source;
                    *real_guard = Some(rebuilt.duplication);
                }
                Err(e) => {
                    self.stats_recovery_failures.fetch_add(1, Ordering::Relaxed);
                    plog!("[DuplicationProxy::AcquireNextFrame] retrying previously failed recovery, still failing: {e:?}");
                }
            }
        }

        // Straight passthrough to the real instance, held for this call's
        // entire duration (including any recovery below) -- ddagrab only
        // ever calls from one thread at a time, so this never blocks on
        // itself; it exists so GetFramePointerShape/ReleaseFrame (called
        // later, still holding the frame this call locked) always see a
        // consistent `real`.
        let result = match real_guard.as_ref() {
            Some(real) => {
                let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                let raw_out_ptr: *mut Option<IDXGIResource> = &mut resource;
                unsafe { real.AcquireNextFrame(timeoutinmilliseconds, &mut frame_info, raw_out_ptr) }
                    .map(|()| (frame_info, resource))
            }
            None => Err(windows::core::Error::from(DXGI_ERROR_WAIT_TIMEOUT)),
        };

        let final_result = match result {
            Ok((frame_info, resource)) => {
                unsafe { *pframeinfo = frame_info };
                ppdesktopresource.write(resource)?;
                self.stats_hits.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                self.stats_timeouts.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
            Err(e)
                if e.code() == DXGI_ERROR_ACCESS_LOST
                    || e.code() == DXGI_ERROR_ACCESS_DENIED
                    || e.code() == DXGI_ERROR_INVALID_CALL =>
            {
                // ddagrab must NEVER observe this HRESULT (see module doc
                // comment) -- recover synchronously, right here, before
                // returning anything. Drop the dead instance FIRST (see
                // recovery.rs) then attempt to re-duplicate; report
                // WAIT_TIMEOUT either way, since even a freshly recovered
                // instance has no frame ready yet this tick.
                plog!("[DuplicationProxy::AcquireNextFrame] lost access (hr={:?}); recovering inline", e.code());
                *real_guard = None;

                match try_recover() {
                    Ok(rebuilt) => {
                        self.stats_recoveries.fetch_add(1, Ordering::Relaxed);
                        plog!("[DuplicationProxy::AcquireNextFrame] recovered inline");
                        *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(rebuilt.source.clone());
                        *self.source.lock().unwrap() = rebuilt.source;
                        *real_guard = Some(rebuilt.duplication);
                    }
                    Err(e2) => {
                        self.stats_recovery_failures.fetch_add(1, Ordering::Relaxed);
                        plog!("[DuplicationProxy::AcquireNextFrame] inline recovery failed too: {e2:?}");
                    }
                }
                Err(windows::core::Error::from(DXGI_ERROR_WAIT_TIMEOUT))
            }
            Err(e) => Err(e),
        };

        if let Ok(mut window_start) = self.stats_window_start.try_lock() {
            if window_start.elapsed() >= Duration::from_secs(1) {
                let calls = self.stats_calls.swap(0, Ordering::Relaxed);
                let hits = self.stats_hits.swap(0, Ordering::Relaxed);
                let timeouts = self.stats_timeouts.swap(0, Ordering::Relaxed);
                let recoveries = self.stats_recoveries.swap(0, Ordering::Relaxed);
                let recovery_failures = self.stats_recovery_failures.swap(0, Ordering::Relaxed);
                let last_timeout_ms = self.stats_last_timeout_ms.load(Ordering::Relaxed);
                plog!(
                    "[DuplicationProxy::AcquireNextFrame] [stats/1s] calls={calls} hits={hits} \
                     timeouts={timeouts} recoveries={recoveries} recovery_failures={recovery_failures} \
                     timeout_arg_ms={last_timeout_ms}"
                );
                *window_start = Instant::now();
            }
        }

        final_result
    }

    fn GetFrameDirtyRects(&self, _dirtyrectsbuffersize: u32, _pdirtyrectsbuffer: *mut RECT, pdirtyrectsbuffersizerequired: *mut u32) -> Result<()> {
        // ddagrab does not call this (confirmed from source); stub it out.
        unsafe { *pdirtyrectsbuffersizerequired = 0 };
        Ok(())
    }

    fn GetFrameMoveRects(&self, _moverectsbuffersize: u32, _pmoverectbuffer: *mut DXGI_OUTDUPL_MOVE_RECT, pmoverectsbuffersizerequired: *mut u32) -> Result<()> {
        unsafe { *pmoverectsbuffersizerequired = 0 };
        Ok(())
    }

    fn GetFramePointerShape(&self, pointershapebuffersize: u32, ppointershapebuffer: *mut c_void, ppointershapebuffersizerequired: *mut u32, pshapeinfo: *mut DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Result<()> {
        // Straight passthrough -- only valid while the frame AcquireNextFrame
        // just returned is still locked, which holds since ddagrab only
        // calls this between AcquireNextFrame and ReleaseFrame on the same
        // thread, and `real` hasn't changed out from under it in between
        // (no other thread ever touches `real` in this design).
        let real_guard = self.real.lock().unwrap();
        match real_guard.as_ref() {
            Some(real) => {
                let result = unsafe {
                    real.GetFramePointerShape(pointershapebuffersize, ppointershapebuffer, ppointershapebuffersizerequired, pshapeinfo)
                };
                if let Err(e) = &result {
                    plog!(
                        "[DuplicationProxy::GetFramePointerShape] failed: {e:?} (buffersize={pointershapebuffersize})"
                    );
                } else {
                    let info = unsafe { *pshapeinfo };
                    plog!(
                        "[DuplicationProxy::GetFramePointerShape] ok: type={} w={} h={} pitch={}",
                        info.Type, info.Width, info.Height, info.Pitch
                    );
                }
                result
            }
            None => {
                unsafe { *ppointershapebuffersizerequired = 0 };
                plog!("[DuplicationProxy::GetFramePointerShape] real is None; reporting size=0");
                Ok(())
            }
        }
    }

    fn MapDesktopSurface(&self) -> Result<DXGI_MAPPED_RECT> {
        // ddagrab does not call this; stub with E_NOTIMPL rather than
        // routing an unused path through to the real instance.
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn UnMapDesktopSurface(&self) -> Result<()> {
        Ok(())
    }

    fn ReleaseFrame(&self) -> Result<()> {
        // Straight passthrough. If recovery replaced `real` since the
        // matching AcquireNextFrame (only possible if THIS call somehow
        // raced a recovery, which can't happen -- both run on ddagrab's one
        // calling thread, never concurrently), there would be nothing valid
        // to release; but ordering guarantees that can't occur, so this is
        // always releasing the same instance the prior AcquireNextFrame
        // call acquired from.
        let real_guard = self.real.lock().unwrap();
        match real_guard.as_ref() {
            Some(real) => unsafe { real.ReleaseFrame() },
            None => Ok(()),
        }
    }
}

/// Shared recovery entry point for both the "last attempt already failed,
/// retry now" path and the "just observed ACCESS_LOST/DENIED/INVALID_CALL"
/// path above: always re-duplicates on the SAME device/output (never
/// recreates the device from scratch).
///
/// A full `recreate_from_scratch` fallback used to exist here, but was
/// removed after real-world UAC-transition testing (test-harness, 3x
/// secure-desktop switches) proved it actively harmful: ddagrab itself keeps
/// using the ORIGINAL `ID3D11Device` it was handed at filter-init time
/// forever (`dda->device_hwctx->device` in vsrc_ddagrab.c), never learning
/// about a replacement device this proxy might create. When
/// `reduplicate_same_device` failed repeatedly during a secure-desktop
/// transition (expected -- `OpenInputDesktop`/`DuplicateOutput1` legitimately
/// fail while the desktop is switched away) and this proxy fell back to
/// rebuilding the whole device from scratch, the freshly-acquired frame
/// texture lived on a DIFFERENT device than ddagrab's own
/// `ID3D11DeviceContext`. ddagrab's subsequent
/// `ID3D11DeviceContext_CopySubresourceRegion` from that texture is then a
/// cross-device copy, which silently fails and surfaces to ffmpeg as
/// `AVERROR_EXTERNAL` -- observed in practice as ffmpeg exiting early with
/// "Error during demuxing: Generic error in an external library" a few
/// hundred ms after the from-scratch rebuild "succeeded". Retrying only the
/// same-device path avoids this class of failure entirely: it either
/// succeeds using the exact device ddagrab already has open, or it keeps
/// failing (and retrying) until the desktop switch resolves -- which the
/// test-harness run confirmed happens within roughly one second of the
/// desktop returning to "Default".
fn try_recover() -> Result<crate::recovery::Rebuilt> {
    crate::recovery::reduplicate_same_device()
}

// Keep IUnknownImpl referenced for the trait bound docs above; the
// #[implement] macro generates the actual IUnknown/Identity glue.
#[allow(dead_code)]
fn _bounds_check<T: IUnknownImpl>() {}
