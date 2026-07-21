//! Minimal, standalone Desktop Duplication API probe -- no ffmpeg, no proxy
//! DLL, no worker threads. Just: create a D3D11 device, duplicate the
//! primary output, and poll AcquireNextFrame in a tight loop on ONE thread,
//! logging every desktop-name/HRESULT transition. Run this under SYSTEM
//! (PsExec -i -s) while triggering a UAC prompt partway through, to see
//! Desktop Duplication's raw behavior with nothing else in the way.
//!
//! Usage: dda-probe.exe [--strategy none|reattach|reduplicate|full-rebuild] [--seconds N]

use std::time::{Duration, Instant};

use windows::core::{Interface, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput5, IDXGIOutputDuplication,
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strategy {
    /// Do nothing on ACCESS_LOST/ACCESS_DENIED -- just keep calling
    /// AcquireNextFrame on the same, now-presumably-dead instance. This is
    /// the baseline: confirms the instance really does stay dead forever
    /// with zero intervention.
    None,
    /// Re-attach this thread to the current input desktop (OpenInputDesktop
    /// + SetThreadDesktop), then retry AcquireNextFrame on the SAME
    /// duplication instance -- no re-duplication.
    Reattach,
    /// Re-attach, then re-issue DuplicateOutput1 on the SAME IDXGIOutput5 /
    /// ID3D11Device, swap in the new duplication instance, retry.
    Reduplicate,
    /// Re-attach, then rebuild the ENTIRE chain from scratch: new
    /// D3D11CreateDevice, new IDXGIDevice/IDXGIAdapter/IDXGIOutput,
    /// new DuplicateOutput1.
    FullRebuild,
}

impl Strategy {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "reattach" => Some(Self::Reattach),
            "reduplicate" => Some(Self::Reduplicate),
            "full-rebuild" => Some(Self::FullRebuild),
            _ => None,
        }
    }
}

fn main() {
    let mut strategy = Strategy::FullRebuild;
    let mut seconds = 30u64;
    let mut skip_rebuild_while_secure = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strategy" => {
                if let Some(v) = args.next() {
                    strategy = Strategy::parse(&v).unwrap_or_else(|| {
                        eprintln!("unknown strategy {v:?}, using full-rebuild");
                        Strategy::FullRebuild
                    });
                }
            }
            "--seconds" => {
                if let Some(v) = args.next() {
                    seconds = v.parse().unwrap_or(30);
                }
            }
            "--allow-rebuild-while-secure" => skip_rebuild_while_secure = false,
            other => eprintln!("ignoring unknown argument: {other}"),
        }
    }

    println!(
        "[probe] strategy={strategy:?} seconds={seconds} skip_rebuild_while_secure={skip_rebuild_while_secure} tid={}",
        unsafe { GetCurrentThreadId() }
    );

    if let Err(e) = run(strategy, seconds, skip_rebuild_while_secure) {
        eprintln!("[probe] fatal error: {e:?}");
        std::process::exit(1);
    }
}

struct Chain {
    device: ID3D11Device,
    output5: IDXGIOutput5,
    dup: IDXGIOutputDuplication,
}

fn create_chain() -> Result<Chain> {
    unsafe {
        let mut device_opt: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device_opt),
            None,
            None,
        )?;
        let device = device_opt.expect("D3D11CreateDevice succeeded without a device");

        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter: IDXGIAdapter = dxgi_device.GetParent()?;
        let output = adapter.EnumOutputs(0)?;
        let output5: IDXGIOutput5 = output.cast()?;
        let formats = [DXGI_FORMAT_B8G8R8A8_UNORM];
        let dup = output5.DuplicateOutput1(&device, 0, &formats)?;

        Ok(Chain { device, output5, dup })
    }
}

fn reduplicate(chain: &Chain) -> Result<IDXGIOutputDuplication> {
    let formats = [DXGI_FORMAT_B8G8R8A8_UNORM];
    unsafe { chain.output5.DuplicateOutput1(&chain.device, 0, &formats) }
}

fn attach_input_desktop() -> Result<String> {
    unsafe {
        let tid = GetCurrentThreadId();
        let access = DESKTOP_ACCESS_FLAGS(0x01FF);
        let desktop = OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, false, access)?;
        let name = desktop_name(desktop);
        let switch = SetThreadDesktop(desktop);
        let _ = CloseDesktop(desktop);
        switch?;

        if let Ok(current) = GetThreadDesktop(tid) {
            let current_name = desktop_name(current);
            if current_name != name {
                println!("[probe] [tid={tid}] WARNING: OpenInputDesktop said {name:?} but GetThreadDesktop verification shows {current_name:?}");
            }
        }
        Ok(name)
    }
}

unsafe fn desktop_name(desktop: windows::Win32::System::StationsAndDesktops::HDESK) -> String {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    match GetUserObjectInformationW(
        HANDLE(desktop.0),
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

fn is_secure(name: &str) -> bool {
    name.eq_ignore_ascii_case("Winlogon") || name.eq_ignore_ascii_case("Screen-saver")
}

fn run(strategy: Strategy, seconds: u64, skip_rebuild_while_secure: bool) -> Result<()> {
    let tid = unsafe { GetCurrentThreadId() };
    let initial_desktop = attach_input_desktop()?;
    println!("[probe] [tid={tid}] initial input desktop: {initial_desktop:?}");

    let mut chain = create_chain()?;
    println!("[probe] [tid={tid}] initial device chain + duplication created successfully");

    let start = Instant::now();
    let mut frame_count: u64 = 0;
    let mut fail_count: u64 = 0;
    let mut last_desktop_name = initial_desktop;
    let mut last_report = Instant::now();

    while start.elapsed() < Duration::from_secs(seconds) {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let result = unsafe { chain.dup.AcquireNextFrame(200, &mut frame_info, &mut resource) };

        match result {
            Ok(()) => {
                frame_count += 1;
                unsafe {
                    let _ = chain.dup.ReleaseFrame();
                }
            }
            Err(e) => {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    // normal, no new frame this tick
                } else if code == DXGI_ERROR_ACCESS_LOST || code == DXGI_ERROR_ACCESS_DENIED {
                    fail_count += 1;
                    let elapsed = start.elapsed().as_secs_f64();
                    println!("[probe] [tid={tid}] t={elapsed:.2}s AcquireNextFrame failed: {code:?} (fail #{fail_count}, frames so far: {frame_count})");

                    match strategy {
                        Strategy::None => {
                            // do nothing; keep hammering the same dead instance
                        }
                        Strategy::Reattach => match attach_input_desktop() {
                            Ok(name) => {
                                if name != last_desktop_name {
                                    println!("[probe] [tid={tid}] desktop changed: {last_desktop_name:?} -> {name:?}");
                                    last_desktop_name = name;
                                }
                            }
                            Err(e) => println!("[probe] [tid={tid}] attach_input_desktop failed: {e:?}"),
                        },
                        Strategy::Reduplicate => {
                            let name = attach_input_desktop().unwrap_or_default();
                            if skip_rebuild_while_secure && is_secure(&name) {
                                println!("[probe] [tid={tid}] desktop is secure ({name:?}); skipping re-duplicate this tick");
                            } else {
                                match reduplicate(&chain) {
                                    Ok(new_dup) => {
                                        chain.dup = new_dup;
                                        println!("[probe] [tid={tid}] re-duplicate succeeded (desktop={name:?})");
                                    }
                                    Err(e) => println!("[probe] [tid={tid}] re-duplicate failed (desktop={name:?}): {e:?}"),
                                }
                            }
                        }
                        Strategy::FullRebuild => {
                            let name = attach_input_desktop().unwrap_or_default();
                            if skip_rebuild_while_secure && is_secure(&name) {
                                println!("[probe] [tid={tid}] desktop is secure ({name:?}); skipping full rebuild this tick");
                            } else {
                                match create_chain() {
                                    Ok(new_chain) => {
                                        chain = new_chain;
                                        println!("[probe] [tid={tid}] full rebuild succeeded (desktop={name:?})");
                                    }
                                    Err(e) => println!("[probe] [tid={tid}] full rebuild failed (desktop={name:?}): {e:?}"),
                                }
                            }
                        }
                    }
                } else {
                    println!("[probe] [tid={tid}] AcquireNextFrame failed with UNEXPECTED error: {code:?}");
                }
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            println!(
                "[probe] [tid={tid}] t={:.1}s frames={frame_count} fails={fail_count}",
                start.elapsed().as_secs_f64()
            );
            last_report = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(16));
    }

    println!("[probe] [tid={tid}] done: total_frames={frame_count} total_fails={fail_count}");
    Ok(())
}
