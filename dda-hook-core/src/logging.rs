use std::fmt::Arguments;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);

    let mut guard = LOG_FILE.lock().unwrap();
    if guard.is_some() {
        return;
    }
    let path = log_path();
    *guard = OpenOptions::new().create(true).append(true).open(path).ok();
}

fn log_path() -> std::path::PathBuf {
    // Next to the DLL itself when possible, falling back to TEMP.
    if let Ok(mut p) = std::env::current_exe() {
        p.pop();
        p.push("dda_hook_core.log");
        return p;
    }
    std::env::temp_dir().join("dda_hook_core.log")
}

pub fn log(args: Arguments<'_>) {
    let elapsed_ms = START.get().map(|s| s.elapsed().as_millis()).unwrap_or(0);
    let line = format!("[dda_hook_core] [t={elapsed_ms}ms] {args}\n");

    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }

    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

macro_rules! plog {
    ($($arg:tt)*) => {
        $crate::logging::log(format_args!($($arg)*))
    };
}

pub(crate) use plog;
