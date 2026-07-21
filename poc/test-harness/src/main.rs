use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

#[derive(Parser)]
#[command(about = "Runs ffmpeg's ddagrab filter, triggers a UAC-style secure-desktop switch, \
                    and watches the proxy DLL's log for recovery.")]
struct Cli {
    #[arg(long, default_value = "ffmpeg-master-latest-win64-lgpl-shared/bin/ffmpeg.exe")]
    ffmpeg_exe: PathBuf,

    /// How long to run the capture, in seconds.
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,

    /// Seconds after start to trigger the first UAC-style desktop switch.
    #[arg(long, default_value_t = 5)]
    trigger_after_secs: u64,

    /// How many UAC-style prompts to trigger during the run.
    #[arg(long, default_value_t = 3)]
    trigger_count: u32,

    /// Seconds between successive triggers.
    #[arg(long, default_value_t = 5)]
    trigger_interval_secs: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let bin_dir = cli
        .ffmpeg_exe
        .parent()
        .context("ffmpeg_exe has no parent directory")?
        .to_path_buf();
    let log_path = bin_dir.join("ddagrab_proxy.log");
    let _ = fs::remove_file(&log_path); // start each run with a clean log

    println!("starting ffmpeg ({}) with ddagrab for {}s...", cli.ffmpeg_exe.display(), cli.duration_secs);
    let mut ffmpeg = spawn_ffmpeg(&cli.ffmpeg_exe, cli.duration_secs)?;

    let start = Instant::now();
    let mut triggered = 0u32;
    let mut next_trigger_at = Duration::from_secs(cli.trigger_after_secs);

    loop {
        if let Some(status) = ffmpeg.try_wait()? {
            println!("ffmpeg exited early with status {status:?} after {:?}", start.elapsed());
            break;
        }

        let elapsed = start.elapsed();
        if triggered < cli.trigger_count && elapsed >= next_trigger_at {
            triggered += 1;
            println!("[{:?}] triggering UAC-style secure-desktop switch ({triggered}/{})", elapsed, cli.trigger_count);
            if let Err(e) = trigger_uac_prompt() {
                eprintln!("failed to trigger UAC prompt: {e:?}");
            }
            next_trigger_at = elapsed + Duration::from_secs(cli.trigger_interval_secs);
        }

        if elapsed >= Duration::from_secs(cli.duration_secs + 10) {
            println!("timed out waiting for ffmpeg to exit; killing it");
            let _ = ffmpeg.kill();
            break;
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    let _ = ffmpeg.wait();

    println!("\n--- ddagrab_proxy.log ({}) ---", log_path.display());
    print_log(&log_path)?;

    Ok(())
}

fn spawn_ffmpeg(ffmpeg_exe: &PathBuf, duration_secs: u64) -> Result<Child> {
    Command::new(ffmpeg_exe)
        .args([
            "-hide_banner",
            "-f",
            "lavfi",
            "-i",
            "ddagrab",
            "-t",
            &duration_secs.to_string(),
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn ffmpeg")
}

/// Triggers a genuine UAC secure-desktop transition by requesting elevation
/// on a harmless built-in exe. Whether the user approves, denies, or the
/// prompt times out, the desktop switch itself (which is what breaks
/// AcquireNextFrame) already happened by the time the prompt is shown.
fn trigger_uac_prompt() -> Result<()> {
    unsafe {
        let operation = HSTRING::from("runas");
        let file = HSTRING::from("cmd.exe");
        let params = HSTRING::from("/c exit");

        let result = ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );

        // ShellExecuteW returns a value <= 32 on failure; the secure-desktop
        // switch already happened by the time the consent dialog appears, so
        // even a value indicating "user hasn't responded yet" is fine here.
        if result.0 as isize <= 32 {
            anyhow::bail!("ShellExecuteW(runas) returned failure code {}", result.0 as isize);
        }
    }
    Ok(())
}

fn print_log(log_path: &PathBuf) -> Result<()> {
    match fs::File::open(log_path) {
        Ok(file) => {
            for line in BufReader::new(file).lines() {
                println!("{}", line?);
            }
        }
        Err(e) => {
            println!("(could not open log: {e})");
        }
    }
    Ok(())
}
