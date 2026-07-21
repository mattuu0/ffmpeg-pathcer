//! Hijacks the REAL `IDXGIOutputDuplication` instance that ddagrab itself
//! creates (via the hooked DuplicateOutput/DuplicateOutput1) and hands it to
//! a dedicated background thread that polls it continuously, independent of
//! whenever ddagrab happens to call `AcquireNextFrame` on the proxy stub we
//! install in its place (see `duplication_proxy.rs`). Each captured frame is
//! copied (GPU-side only, no CPU readback) into a shared cache; ddagrab's own
//! AcquireNextFrame calls are served from that cache, getting whatever the
//! latest frame is whenever they ask.
//!
//! Recovery on ACCESS_LOST/ACCESS_DENIED/INVALID_CALL: drop the dead
//! instance FIRST, then re-duplicate on the same device/output (falling back
//! to a full from-scratch device rebuild only if that itself fails).
//! Confirmed via extensive logging that recreating a new DuplicateOutput1
//! instance WITHOUT first dropping the previous (dead) one -- even though
//! DXGI reports success -- produces an instance whose own AcquireNextFrame
//! then fails forever, including long after the desktop returns to Default.
//! DXGI only tracks one live IDXGIOutputDuplication per output at a time, so
//! the old COM object's Release() must actually run before requesting the
//! replacement. This is also why a second, independently-polling instance in
//! the same process (as a standalone `desktop-shot` run would create)
//! cannot coexist with ddagrab's own -- confirmed via logging that starting
//! one while ddagrab already holds a live instance fails immediately with
//! E_INVALIDARG. Hijacking ddagrab's own instance instead of creating a
//! second one sidesteps that conflict entirely.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK, HDESK, UOI_NAME,
};

use crate::logging::plog;
use crate::state::DuplicationSource;

/// How long each AcquireNextFrame call on the real instance blocks for
/// before timing out with "no new frame yet". Short enough that recovery
/// (post drop-then-reduplicate) reacts quickly, long enough to not busy-loop
/// the CPU while idle -- ddagrab's own frame arrival cadence (its requested
/// framerate) governs how often a frame actually arrives sooner than this.
const PUMP_ACQUIRE_TIMEOUT_MS: u32 = 200;

/// A cached copy of the most recently captured desktop frame, GPU-side, plus
/// a monotonically increasing generation counter so callers can tell "is
/// this a frame I haven't seen yet" without comparing pixel data.
struct CachedFrame {
    texture: ID3D11Texture2D,
    frame_info: DXGI_OUTDUPL_FRAME_INFO,
}

/// The real cursor shape data fetched from the real GetFramePointerShape the
/// one time DXGI_OUTDUPL_FRAME_INFO::PointerShapeBufferSize > 0 on some
/// AcquireNextFrame -- DXGI only reports "new shape available" on the frame
/// where the cursor actually changed, not every frame, so this needs to be
/// held onto until ddagrab's own GetFramePointerShape call consumes it.
struct CachedPointerShape {
    buf: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

/// A GPU texture reused across calls as long as the source frame's
/// dimensions haven't changed, so steady-state capture (the overwhelmingly
/// common case -- desktop resolution rarely changes mid-capture) never calls
/// `CreateTexture2D` per frame. `CreateTexture2D` involves driver-side
/// allocation and was confirmed (via observed encoder fps/drop counts) to be
/// the dominant cost keeping this pipeline well below the requested 60fps --
/// two such allocations happened per frame before this (once copying the
/// real frame into the cache, once copying the cache into ddagrab's own
/// resource), which is exactly the kind of per-frame allocation this avoids.
struct ReusableTexture {
    tex: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

impl ReusableTexture {
    const fn new() -> Self {
        Self { tex: None, width: 0, height: 0 }
    }

    /// Returns a texture matching `src`'s current dimensions, creating (or
    /// re-creating, if the size changed) one only when necessary.
    fn get_or_create(&mut self, device: &ID3D11Device, src_desc: &D3D11_TEXTURE2D_DESC) -> Option<&ID3D11Texture2D> {
        if self.tex.is_none() || self.width != src_desc.Width || self.height != src_desc.Height {
            let mut desc = *src_desc;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;

            let mut out: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut out)) }.ok()?;
            self.tex = out;
            self.width = src_desc.Width;
            self.height = src_desc.Height;
        }
        self.tex.as_ref()
    }
}

pub struct FrameCache {
    latest: Mutex<Option<CachedFrame>>,
    /// The most recently fetched real cursor shape, held until ddagrab's own
    /// GetFramePointerShape call consumes it (see CachedPointerShape doc).
    pointer_shape: Mutex<Option<CachedPointerShape>>,
    /// Reused for the copy ddagrab's own AcquireNextFrame hands out --
    /// separate from the pump thread's own reusable texture below since both
    /// can be written/read close together in time.
    output_tex: Mutex<ReusableTexture>,
    /// `ID3D11DeviceContext` (the immediate context) is NOT thread-safe --
    /// calling it concurrently from the pump thread (writing fresh frames
    /// into the cache) and whatever thread ddagrab calls AcquireNextFrame
    /// from (reading the cache into `output_tex`) at the same time is
    /// undefined behavior (data races / driver corruption / crashes), even
    /// though both call sites happened to not visibly crash before this was
    /// caught. This lock serializes every CreateTexture2D/CopyResource call
    /// in this module so only one thread ever touches the context at a time.
    gpu_ctx_lock: Mutex<()>,
    generation: AtomicU64,
    desc: DXGI_OUTDUPL_DESC,
    device: ID3D11Device,
}

impl FrameCache {
    /// Returns the latest cached frame as a fresh `IDXGIResource`, plus its
    /// `DXGI_OUTDUPL_FRAME_INFO`, IF it's newer than `last_seen_generation`.
    /// Returns `None` if there's no frame yet, or nothing newer than what the
    /// caller already has.
    ///
    /// The returned resource is a copy of the cache into `output_tex`, since
    /// `AcquireNextFrame`'s contract hands the caller a resource it can hold
    /// until `ReleaseFrame` -- handing out the single cache texture directly
    /// would let ddagrab observe the pump thread overwriting it mid-read.
    pub fn try_take_latest(&self, last_seen_generation: u64) -> Option<(u64, DXGI_OUTDUPL_FRAME_INFO, IDXGIResource)> {
        let current_gen = self.generation.load(Ordering::Acquire);
        if current_gen == last_seen_generation {
            return None;
        }

        let guard = self.latest.lock().unwrap();
        let cached = guard.as_ref()?;

        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { cached.texture.GetDesc(&mut src_desc) };

        let _gpu_guard = self.gpu_ctx_lock.lock().unwrap();
        let mut output_tex = self.output_tex.lock().unwrap();
        let out = output_tex.get_or_create(&self.device, &src_desc)?;
        let ctx = unsafe { self.device.GetImmediateContext() }.ok()?;
        unsafe { ctx.CopyResource(out, &cached.texture) };
        let resource: IDXGIResource = out.cast().ok()?;
        Some((current_gen, cached.frame_info, resource))
    }

    pub fn desc(&self) -> DXGI_OUTDUPL_DESC {
        self.desc
    }

    /// Copies `src` into the pump thread's own reusable cache-write texture,
    /// re-creating it only if `src`'s dimensions changed since last time.
    fn clone_into_cache_texture(&self, cache_tex: &mut ReusableTexture, src: &ID3D11Texture2D) -> Option<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };

        let _gpu_guard = self.gpu_ctx_lock.lock().unwrap();
        let out = cache_tex.get_or_create(&self.device, &desc)?;
        let ctx = unsafe { self.device.GetImmediateContext() }.ok()?;
        unsafe { ctx.CopyResource(out, src) };
        Some(out.clone())
    }

    fn publish(&self, texture: ID3D11Texture2D, frame_info: DXGI_OUTDUPL_FRAME_INFO) {
        *self.latest.lock().unwrap() = Some(CachedFrame { texture, frame_info });
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Stashes cursor shape data fetched from the real GetFramePointerShape,
    /// for ddagrab's own GetFramePointerShape call to consume later.
    fn publish_pointer_shape(&self, buf: Vec<u8>, info: DXGI_OUTDUPL_POINTER_SHAPE_INFO) {
        *self.pointer_shape.lock().unwrap() = Some(CachedPointerShape { buf, info });
    }

    /// Returns the cached cursor shape's buffer length without consuming it,
    /// so a too-small caller buffer can be told the required size (matching
    /// GetFramePointerShape's DXGI_ERROR_MORE_DATA contract) without losing
    /// the shape data for a follow-up call with a bigger buffer.
    pub fn peek_pointer_shape_len(&self) -> Option<usize> {
        self.pointer_shape.lock().unwrap().as_ref().map(|p| p.buf.len())
    }

    /// Takes (removes) the cached cursor shape, if any -- consumed exactly
    /// once, matching the real DDA's own "only reported when it actually
    /// changed" semantics: ddagrab should only see PointerShapeBufferSize > 0
    /// on the one AcquireNextFrame call after this, not indefinitely.
    pub fn take_pointer_shape(&self) -> Option<(Vec<u8>, DXGI_OUTDUPL_POINTER_SHAPE_INFO)> {
        self.pointer_shape.lock().unwrap().take().map(|p| (p.buf, p.info))
    }
}

/// Starts the pump thread and returns the shared cache ddagrab's own
/// AcquireNextFrame calls read from. `real`/`source` is the first
/// successfully created real duplication instance (from the hooked
/// DuplicateOutput/DuplicateOutput1 call).
pub fn start(real: IDXGIOutputDuplication, source: DuplicationSource) -> Arc<FrameCache> {
    let device = device_of(&source);
    let desc = unsafe { real.GetDesc() };

    let cache = Arc::new(FrameCache {
        latest: Mutex::new(None),
        pointer_shape: Mutex::new(None),
        output_tex: Mutex::new(ReusableTexture::new()),
        gpu_ctx_lock: Mutex::new(()),
        generation: AtomicU64::new(0),
        desc,
        device,
    });

    let cache_for_thread = cache.clone();
    std::thread::Builder::new()
        .name("ddagrab_proxy_pump".into())
        .spawn(move || run(cache_for_thread, real, source))
        .expect("failed to spawn dedicated Desktop Duplication pump thread");

    cache
}

fn attach_input_desktop() -> windows::core::Result<String> {
    unsafe {
        let access = DESKTOP_ACCESS_FLAGS(0x01FF);
        let desktop = OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, false, access)?;
        let name = desktop_name(desktop);
        let switch = SetThreadDesktop(desktop);
        let _ = CloseDesktop(desktop);
        switch?;
        Ok(name)
    }
}

unsafe fn desktop_name(desktop: HDESK) -> String {
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
        Err(e) => format!("unknown-{e:?}"),
    }
}

fn device_of(source: &DuplicationSource) -> ID3D11Device {
    match source {
        DuplicationSource::V1 { device, .. } => device.clone(),
        DuplicationSource::V5 { device, .. } => device.clone(),
    }
}

fn run(cache: Arc<FrameCache>, real: IDXGIOutputDuplication, mut source: DuplicationSource) {
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    plog!("[pump tid={tid}] dedicated Desktop Duplication pump thread started");
    crate::recovery::attach_once_this_thread("pump startup");

    let mut last_desktop_name = attach_input_desktop().unwrap_or_else(|e| {
        plog!("[pump tid={tid}] attach_input_desktop failed: {e:?}");
        "Default".to_string()
    });

    let mut real: Option<IDXGIOutputDuplication> = Some(real);
    let mut cache_write_tex = ReusableTexture::new();

    loop {
        let desktop_name = attach_input_desktop().unwrap_or_else(|e| {
            plog!("[pump tid={tid}] attach_input_desktop failed: {e:?}");
            last_desktop_name.clone()
        });
        if desktop_name != last_desktop_name {
            plog!("[pump tid={tid}] desktop changed: {last_desktop_name:?} -> {desktop_name:?}");
            last_desktop_name = desktop_name.clone();
        }

        let acquire_result = real.as_ref().map(|r| {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let raw_out_ptr: *mut Option<IDXGIResource> = &mut resource;
            let hr = unsafe { r.AcquireNextFrame(PUMP_ACQUIRE_TIMEOUT_MS, &mut frame_info, raw_out_ptr) };
            (hr, frame_info, resource)
        });

        match acquire_result {
            None => {
                // No live instance right now (previous recovery attempt
                // failed) -- try to get one before the next tick.
                let (new_real, new_source) = reduplicate(source, tid);
                real = new_real;
                source = new_source;
            }
            Some((Ok(()), frame_info, resource)) => {
                // PointerShapeBufferSize > 0 means DXGI is reporting a new
                // cursor shape THIS tick -- it's only reported on the frame
                // where the cursor actually changed, not every frame, so
                // this must be fetched now (GetFramePointerShape is only
                // valid while the frame is still locked, i.e. before
                // ReleaseFrame below) and stashed for ddagrab's own
                // GetFramePointerShape call to pick up later. ddagrab
                // (confirmed via its own "Unsupported pointer shape type"
                // error text) does call the real GetFramePointerShape and
                // composites the cursor into the frame itself when draw_mouse
                // is enabled (its default) -- so as long as this proxy hands
                // back genuine shape data instead of always claiming
                // "nothing new", ddagrab draws the cursor exactly as it
                // would with the real, un-hijacked instance.
                if frame_info.PointerShapeBufferSize > 0 {
                    if let Some(real) = real.as_ref() {
                        match fetch_pointer_shape(real, frame_info.PointerShapeBufferSize) {
                            Ok((buf, shape_info)) => cache.publish_pointer_shape(buf, shape_info),
                            Err(e) => plog!("[pump tid={tid}] GetFramePointerShape failed: {e:?}"),
                        }
                    }
                }

                if let Some(resource) = resource {
                    if let Ok(tex) = resource.cast::<ID3D11Texture2D>() {
                        if let Some(copy) = cache.clone_into_cache_texture(&mut cache_write_tex, &tex) {
                            cache.publish(copy, frame_info);
                        }
                    }
                }
                unsafe {
                    if let Err(e) = real.as_ref().unwrap().ReleaseFrame() {
                        plog!("[pump tid={tid}] ReleaseFrame failed: {e:?}");
                    }
                }
            }
            Some((Err(e), _, _)) => {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    // Normal: no new frame this tick.
                } else if code == DXGI_ERROR_ACCESS_LOST
                    || code == DXGI_ERROR_ACCESS_DENIED
                    || code == DXGI_ERROR_INVALID_CALL
                {
                    plog!("[pump tid={tid}] AcquireNextFrame lost access (hr={code:?}); reduplicating");
                    // Drop the dead instance FIRST, before asking DXGI for a
                    // new one -- see module doc comment for why this order
                    // matters.
                    real = None;
                    let (new_real, new_source) = reduplicate(source, tid);
                    real = new_real;
                    source = new_source;
                } else {
                    plog!("[pump tid={tid}] AcquireNextFrame failed with unexpected error: {code:?}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Fetches the real cursor shape data via the real GetFramePointerShape.
/// Must be called BEFORE ReleaseFrame -- GetFramePointerShape is only valid
/// while the frame that reported the new shape is still locked.
fn fetch_pointer_shape(
    real: &IDXGIOutputDuplication,
    buffer_size_hint: u32,
) -> windows::core::Result<(Vec<u8>, DXGI_OUTDUPL_POINTER_SHAPE_INFO)> {
    let mut buf = vec![0u8; buffer_size_hint as usize];
    let mut size_required = 0u32;
    let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();

    unsafe {
        real.GetFramePointerShape(
            buffer_size_hint,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size_required,
            &mut shape_info,
        )?;
    }
    buf.truncate(size_required as usize);
    Ok((buf, shape_info))
}

/// Recovers the real duplication instance after ACCESS_LOST/ACCESS_DENIED/
/// INVALID_CALL: try re-duplicating on the same device/output first, falling
/// back to a full from-scratch device rebuild only if that itself fails.
/// Backs off briefly after any successful recovery so this thread's own
/// independent polling loop never hammers the GPU/driver back-to-back.
///
/// The caller must have ALREADY dropped the previous (dead) instance before
/// calling this -- DXGI only tracks one live IDXGIOutputDuplication per
/// output at a time, so requesting a new one while the old COM object (even
/// a dead one, pending Release()) is still alive produces an instance whose
/// own AcquireNextFrame then fails forever.
fn reduplicate(source: DuplicationSource, tid: u32) -> (Option<IDXGIOutputDuplication>, DuplicationSource) {
    *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(source.clone());

    match crate::recovery::reduplicate_same_device() {
        Ok(rebuilt) => {
            *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(rebuilt.source.clone());
            plog!("[pump tid={tid}] reduplicate_same_device succeeded");
            std::thread::sleep(Duration::from_millis(50));
            (Some(rebuilt.duplication), rebuilt.source)
        }
        Err(e) => {
            plog!("[pump tid={tid}] reduplicate_same_device failed ({e:?}); trying full rebuild");
            match crate::recovery::recreate_from_scratch() {
                Ok(rebuilt) => {
                    *crate::state::LAST_DUPLICATION_SOURCE.lock() = Some(rebuilt.source.clone());
                    plog!("[pump tid={tid}] full rebuild succeeded");
                    std::thread::sleep(Duration::from_millis(50));
                    (Some(rebuilt.duplication), rebuilt.source)
                }
                Err(e) => {
                    plog!("[pump tid={tid}] full rebuild also failed ({e:?}); backing off");
                    std::thread::sleep(Duration::from_millis(200));
                    (None, source)
                }
            }
        }
    }
}
