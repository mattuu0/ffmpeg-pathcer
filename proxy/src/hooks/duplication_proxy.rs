use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::{implement, IUnknownImpl, Interface, OutRef, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    IDXGIObject_Impl, IDXGIOutputDuplication, IDXGIOutputDuplication_Impl, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAPPED_RECT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

use crate::hooks::pump::{self, FrameCache};
use crate::state::DuplicationSource;

/// A COM object implementing `IDXGIOutputDuplication` that ddagrab holds and
/// calls, but which never touches the real underlying `IDXGIOutputDuplication`
/// itself. A dedicated background `pump` thread (see `pump.rs`) owns the real
/// instance, continuously re-acquiring frames from it on its own independent
/// schedule and recovering from ACCESS_LOST at its own pace; this proxy just
/// hands ddagrab whatever the pump's latest cached frame is.
///
/// Why: routing ddagrab's own AcquireNextFrame calls straight through to the
/// real instance never recovered after a UAC secure-desktop transition, no
/// matter how the recovery itself was implemented -- every attempt reported
/// success, but the very next AcquireNextFrame on the "recovered" instance
/// itself failed with ACCESS_LOST again, indefinitely. Decoupling ddagrab's
/// polling entirely from the real instance's recovery timing means ddagrab
/// only ever sees "frame available" or "no new frame yet" -- it never
/// observes ACCESS_LOST or a recovery in progress at all. See `pump.rs`'s
/// module doc comment for the (different, and more subtle) bug that made
/// recovery itself actually work: dropping the dead instance before
/// requesting its replacement.
#[implement(IDXGIOutputDuplication)]
pub struct DuplicationProxy {
    cache: Arc<FrameCache>,
    last_seen_generation: AtomicU64,
}

/// Replaces `*ppoutputduplication` (a real `IDXGIOutputDuplication*` freshly
/// returned by `DuplicateOutput`/`DuplicateOutput1`) with a `DuplicationProxy`
/// backed by a newly started pump thread, which takes over the real instance.
///
/// # Safety
/// `ppoutputduplication` must point at a valid, just-returned
/// `IDXGIOutputDuplication*` matching a successful HRESULT.
pub unsafe fn wrap_duplication(ppoutputduplication: *mut *mut c_void, source: DuplicationSource) {
    let raw = *ppoutputduplication;
    // Takes over the single owning ref the real DuplicateOutput call already
    // produced at `raw` -- no AddRef needed.
    let real: IDXGIOutputDuplication = Interface::from_raw(raw);

    let cache = pump::start(real, source);
    let proxy = DuplicationProxy { cache, last_seen_generation: AtomicU64::new(0) };
    let com_proxy: IDXGIOutputDuplication = proxy.into();

    // Hand out our wrapper in place of the real pointer; the caller (ddagrab)
    // never sees the real object's address.
    *ppoutputduplication = com_proxy.into_raw();
}

impl IDXGIObject_Impl for DuplicationProxy_Impl {
    // ddagrab never calls IDXGIObject methods on the duplication instance
    // (confirmed from source) -- stub these out rather than routing an
    // unused path through the pump.
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
        unsafe { *pdesc = self.cache.desc() };
    }

    fn AcquireNextFrame(
        &self,
        timeoutinmilliseconds: u32,
        pframeinfo: *mut DXGI_OUTDUPL_FRAME_INFO,
        ppdesktopresource: OutRef<'_, IDXGIResource>,
    ) -> Result<()> {
        // Poll the pump's cache for up to `timeoutinmilliseconds`, matching
        // AcquireNextFrame's normal blocking-with-timeout contract, rather
        // than checking once and immediately reporting "nothing yet" --
        // ddagrab's own poll loop passes a real timeout here and expects
        // this call to actually wait roughly that long before giving up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeoutinmilliseconds as u64);
        loop {
            let last_seen = self.last_seen_generation.load(Ordering::Acquire);
            if let Some((generation, frame_info, resource)) = self.cache.try_take_latest(last_seen) {
                self.last_seen_generation.store(generation, Ordering::Release);
                unsafe { *pframeinfo = frame_info };
                ppdesktopresource.write(Some(resource))?;
                return Ok(());
            }

            if std::time::Instant::now() >= deadline {
                // ddagrab (libavfilter/vsrc_ddagrab.c, next_frame_internal)
                // special-cases ONLY DXGI_ERROR_WAIT_TIMEOUT as "no frame
                // yet, try again" (maps to AVERROR(EAGAIN), which its
                // callers loop on). Every other failure kills the filter
                // outright with zero recovery (confirmed from ddagrab's
                // source) -- this proxy never surfaces ACCESS_LOST/
                // ACCESS_DENIED to ddagrab at all; the pump absorbs those
                // entirely on its own independent schedule (see pump.rs).
                return Err(windows::core::Error::from(DXGI_ERROR_WAIT_TIMEOUT));
            }

            std::thread::sleep(std::time::Duration::from_millis(2));
        }
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
        // The pump thread is the one that actually calls the real
        // GetFramePointerShape (on the AcquireNextFrame tick where DXGI
        // reports PointerShapeBufferSize > 0 -- it's only valid while that
        // frame is still locked) and stashes the result here. ddagrab does
        // call this and composites the cursor into the frame itself when
        // draw_mouse is enabled (its default), so serving genuine cached
        // shape data -- rather than always claiming "nothing new" -- is what
        // makes the cursor show up in captures again.
        // Peek the length first without consuming the cached shape -- if the
        // caller's buffer is too small we must report DXGI_ERROR_MORE_DATA
        // and leave the shape in the cache so a follow-up call with a bigger
        // buffer can still retrieve it (matching GetFramePointerShape's
        // documented contract). Only `take` (consuming) once we know the
        // buffer is big enough to actually hold it.
        match self.cache.peek_pointer_shape_len() {
            Some(len) if (pointershapebuffersize as usize) < len => {
                unsafe { *ppointershapebuffersizerequired = len as u32 };
                Err(windows::core::Error::from(
                    windows::Win32::Graphics::Dxgi::DXGI_ERROR_MORE_DATA,
                ))
            }
            Some(_) => {
                let (buf, shape_info) = self.cache.take_pointer_shape().unwrap();
                unsafe {
                    std::ptr::copy_nonoverlapping(buf.as_ptr(), ppointershapebuffer as *mut u8, buf.len());
                    *ppointershapebuffersizerequired = buf.len() as u32;
                    *pshapeinfo = shape_info;
                }
                Ok(())
            }
            None => {
                unsafe { *ppointershapebuffersizerequired = 0 };
                Ok(())
            }
        }
    }

    fn MapDesktopSurface(&self) -> Result<DXGI_MAPPED_RECT> {
        // ddagrab does not call this; stub with E_NOTIMPL rather than
        // routing an unused path through the pump.
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn UnMapDesktopSurface(&self) -> Result<()> {
        Ok(())
    }

    fn ReleaseFrame(&self) -> Result<()> {
        // No-op: the pump thread already released the REAL frame itself,
        // immediately after copying it into the cache. The resource handed
        // to ddagrab is our own private copy, not the real locked frame, so
        // there is nothing left for ddagrab's ReleaseFrame to do.
        Ok(())
    }
}

// Keep IUnknownImpl referenced for the trait bound docs above; the
// #[implement] macro generates the actual IUnknown/Identity glue.
#[allow(dead_code)]
fn _bounds_check<T: IUnknownImpl>() {}
