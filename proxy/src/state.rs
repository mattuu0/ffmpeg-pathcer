use std::ffi::c_void;

use parking_lot::Mutex;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, D3D11_CREATE_DEVICE_FLAG};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput1, IDXGIOutput5};

/// The exact arguments avutil's `hwcontext_d3d11va.c` passed to the very
/// first successful `D3D11CreateDevice` call, stashed so a full re-init
/// (device -> IDXGIDevice -> IDXGIAdapter -> IDXGIOutput -> duplication) can
/// reproduce it from scratch after a UAC secure-desktop transition, rather
/// than reusing objects that were bound to a desktop that may no longer be
/// current. `padapter` is null in the common case (ddagrab/avutil let D3D11
/// pick the default adapter); if non-null it's an `IDXGIAdapter*` we don't
/// own a extra ref on, so it's only safe to reuse for as long as the
/// original device (and thus the adapter) is assumed still alive -- which
/// holds here since we only ever re-create using the same process-lifetime
/// adapter selection.
#[derive(Clone)]
pub struct DeviceCreateArgs {
    pub adapter: *mut c_void,
    pub driver_type: D3D_DRIVER_TYPE,
    pub software: HMODULE,
    pub flags: D3D11_CREATE_DEVICE_FLAG,
    pub feature_levels: Vec<i32>,
    pub sdk_version: u32,
}

unsafe impl Send for DeviceCreateArgs {}
unsafe impl Sync for DeviceCreateArgs {}

pub static DEVICE_CREATE_ARGS: Mutex<Option<DeviceCreateArgs>> = Mutex::new(None);

/// What's needed to re-run DuplicateOutput/DuplicateOutput1 after an
/// ACCESS_LOST/ACCESS_DENIED, stashed at the moment the original
/// DuplicateOutput(1) call succeeded, so recovery can reproduce the exact
/// same call.
#[derive(Clone)]
pub enum DuplicationSource {
    V1 {
        output1: IDXGIOutput1,
        device: ID3D11Device,
    },
    V5 {
        output5: IDXGIOutput5,
        device: ID3D11Device,
        flags: u32,
        supported_formats: Vec<DXGI_FORMAT>,
    },
}

unsafe impl Send for DuplicationSource {}

/// The device/output pairing behind the most recently *known-good* (at least
/// once successfully produced) `IDXGIOutputDuplication`, kept up to date by
/// both the initial hooked DuplicateOutput/DuplicateOutput1 call and every
/// successful recovery. `recovery::reduplicate_same_device` reads this to
/// re-issue DuplicateOutput(1) on the SAME device/output rather than
/// creating a new device -- see that function's doc comment for why re-using
/// the device is the recovery path that actually works.
pub static LAST_DUPLICATION_SOURCE: Mutex<Option<DuplicationSource>> = Mutex::new(None);

/// Global hook-installation lock: only one thread may install the
/// D3D11CreateDevice hook / walk the QueryInterface->GetAdapter->EnumOutputs
/// chain at a time, since these run at most once or twice per process
/// lifetime and contention here is not a performance concern.
pub static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// DXGI allows only ONE live `IDXGIOutputDuplication` per output at a time --
/// a second concurrent `DuplicateOutput`/`DuplicateOutput1` call while
/// another instance for the same output is still alive is exactly the kind
/// of situation that produces an instance whose own AcquireNextFrame then
/// fails forever. There are three call sites that can invoke the REAL
/// DuplicateOutput/DuplicateOutput1: ddagrab's own initial call (hooked in
/// dxgi_output.rs), and the pump thread's two recovery paths
/// (`reduplicate_same_device` / `recreate_from_scratch`). Holding this lock
/// across every one of them guarantees at most one real duplication attempt
/// -- and therefore at most one live real IDXGIOutputDuplication instance --
/// exists at any instant, process-wide, no matter which thread is driving
/// recovery or how ddagrab itself happens to call in.
pub static DUPLICATE_OUTPUT_LOCK: Mutex<()> = Mutex::new(());

pub type RawPtr = *mut c_void;
