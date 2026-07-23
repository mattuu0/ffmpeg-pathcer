//! Adds/removes a Parsec Virtual Display Driver (VDD) display and resolves
//! it to the `output_idx` ddagrab's own `output_idx=N` option expects (i.e.
//! the index `IDXGIAdapter::EnumOutputs` would return it at), so a wrapper
//! script (record_until_ctrlc.py) can capture the newly created virtual
//! display without guessing which index it landed at.
//!
//! parsec-vdd-rust's own `ParsecDisplay::display_index()` (`address - 0x100`)
//! is NOT the same number space as DXGI's EnumOutputs index -- it's an
//! internal VDD slot index. The only reliable way to get ddagrab's
//! `output_idx` is to match `ParsecDisplay.device_name` (a Win32 GDI device
//! name like `\\.\DISPLAY34`, from `EnumDisplayDevicesA`) against
//! `IDXGIOutput::GetDesc().DeviceName` (the exact same string space) while
//! walking `EnumOutputs` ourselves.
//!
//! IMPORTANT: `add` is NOT a one-shot command -- it stays running and keeps
//! the process alive, printing the resolved info on the FIRST line and then
//! calling `vdd_update()` every 100ms for as long as the process lives.
//! Confirmed against the reference implementation this was ported from
//! (rust-castsender's own VDD wrapper): the driver expects a live client to
//! keep calling `vdd_update()` periodically (a "watchdog") for a display it
//! added to keep existing -- a version of this tool that added a display and
//! exited immediately (no watchdog) was observed to have the display
//! disappear again right away. The caller is expected to keep this process
//! running for as long as it wants the virtual display to exist, then send
//! it a "remove\n" line on stdin (or just kill it, though that skips
//! explicit cleanup and relies on Parsec's own driver-side handle-close
//! teardown).

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use clap::Parser;
use parsec_vdd_rust::{
    open_device_handle, query_device_status, vdd_add_and_identify_display, vdd_remove_display,
    vdd_update, VDD_ADAPTER_GUID,
};
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};

// HANDLE is *mut c_void -- not Send by default, but safe here since the
// watchdog thread only ever calls vdd_update() on it and the main thread
// only reads it after joining the watchdog, so there's no concurrent access
// to race. Same justification/pattern as rust-castsender's own SendHandle.
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

#[derive(Parser)]
#[command(about = "Adds a Parsec VDD virtual display, resolves it to a DXGI EnumOutputs index, \
                    then stays running (keeping the display alive via a watchdog) until told \
                    to stop. Send 'remove\\n' on stdin to remove the display and exit cleanly; \
                    otherwise just kill the process (relies on the driver's own handle-close \
                    teardown instead).")]
struct Cli {}

fn main() -> Result<()> {
    let _cli = Cli::parse();

    let status = query_device_status(&parsec_vdd_rust::VDD_CLASS_GUID, parsec_vdd_rust::VDD_HARDWARE_ID);
    let vdd = open_device_handle(&VDD_ADAPTER_GUID)
        .with_context(|| format!("failed to open Parsec VDD device handle (driver status: {status:?})"))?;

    let (monitor_id, display) = vdd_add_and_identify_display(vdd)
        .map_err(|e| anyhow::anyhow!("vdd_add_and_identify_display failed: {e}"))?;

    let (width, height) = display
        .current_mode
        .as_ref()
        .map(|m| (m.width as u32, m.height as u32))
        .unwrap_or((1920, 1080));

    let output_idx = find_dxgi_output_index(&display.device_name).with_context(|| {
        format!(
            "virtual display {:?} was created but no matching IDXGIOutput was found via EnumOutputs",
            display.device_name
        )
    });

    // Best-effort cleanup if we can't resolve the index -- an unresolved
    // virtual display left behind would otherwise require a manual
    // removal the caller doesn't have the monitor_id for (it never saw a
    // successful first line to parse one out of).
    let output_idx = match output_idx {
        Ok(idx) => idx,
        Err(e) => {
            let _ = vdd_remove_display(vdd, monitor_id);
            return Err(e);
        }
    };

    println!("monitor_id={monitor_id} output_idx={output_idx} width={width} height={height}");
    std::io::stdout().flush().ok();

    // Watchdog: call vdd_update() every 100ms for as long as this process
    // lives, exactly matching rust-castsender's own inner::connect()
    // pattern -- without this, the display was observed to disappear again
    // almost immediately after being added.
    let stop_watchdog = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_handle = {
        let stop_watchdog = std::sync::Arc::clone(&stop_watchdog);
        let vdd_for_watchdog = SendHandle(vdd);
        std::thread::spawn(move || {
            let vdd_for_watchdog = vdd_for_watchdog;
            while !stop_watchdog.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = vdd_update(vdd_for_watchdog.0);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
    };

    // Wait for a "remove" command on stdin (the normal shutdown path a
    // caller drives); if stdin closes/EOFs without ever sending it (e.g.
    // the caller was killed), fall through and clean up anyway rather than
    // looping forever.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) if line.trim().eq_ignore_ascii_case("remove") => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    stop_watchdog.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog_handle.join();

    if let Err(e) = vdd_remove_display(vdd, monitor_id) {
        eprintln!("vdd_remove_display failed: {e}");
    }
    println!("removed monitor_id={monitor_id}");

    Ok(())
}

/// Walks the primary D3D11 adapter's outputs (the same enumeration ddagrab
/// itself performs, when it doesn't already have a device -- which is the
/// case here since this is a standalone probe run before ffmpeg starts) and
/// returns the index at which an output's `DeviceName` matches
/// `target_device_name` (`\\.\DISPLAYnn`, exactly the string space
/// `EnumDisplayDevicesA` -- parsec-vdd-rust's own source -- also uses).
fn find_dxgi_output_index(target_device_name: &str) -> Result<u32> {
    let device = create_default_d3d11_device()?;
    let dxgi_device: IDXGIDevice = device.cast().context("ID3D11Device -> IDXGIDevice QueryInterface failed")?;
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetParent() }.context("IDXGIDevice::GetParent(IDXGIAdapter) failed")?;

    let mut index = 0u32;
    loop {
        let output = match unsafe { adapter.EnumOutputs(index) } {
            Ok(o) => o,
            Err(_) => bail!("exhausted EnumOutputs (index {index}) without finding {target_device_name:?}"),
        };
        let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc failed")?;
        let device_name = String::from_utf16_lossy(
            &desc.DeviceName[..desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len())],
        );
        if device_name.eq_ignore_ascii_case(target_device_name) {
            return Ok(index);
        }
        index += 1;
    }
}

fn create_default_d3d11_device() -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Device> {
    unsafe {
        let mut device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(std::ptr::null_mut()),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .context("D3D11CreateDevice failed")?;
        device.context("D3D11CreateDevice succeeded but returned no device")
    }
}
