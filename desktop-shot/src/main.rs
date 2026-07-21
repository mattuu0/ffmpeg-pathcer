//! secure desktop (Winlogon / UAC同意プロンプト / スクリーンセーバー) を含む
//! 各デスクトップをDesktop Duplication API (DDA)でキャプチャできるかを
//! 検証するための最小プログラム。capture-helperと同じくwin_desktop_duplication
//! クレート (AdapterFactory / DesktopDuplicationApi / TextureReader) を使う。
//!
//! 現在の入力デスクトップに (OpenInputDesktop + SetThreadDesktop で) アタッチし、
//! DDAで1フレーム取得してPNGとして保存する、を一定間隔で繰り返す。
//! ファイル名にその時点のデスクトップ名 (Default / Winlogon / Screen-saver 等) を
//! 埋め込むので、デスクトップが切り替わるたびに別ファイルとして残る。
//!
//! SYSTEM権限で実行して初めてWinlogon/secure desktopへのOpenDesktopが成功する
//! (通常ユーザー権限では失敗し、Defaultデスクトップのみキャプチャされる)。
//! 例: PsExec -i -s desktop-shot.exe --interval-ms 500
//!
//! 使い方: desktop-shot.exe [--display-index N] [--out-dir DIR] [--interval-ms N] [--seconds N]

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK, HDESK, UOI_NAME,
};

struct Args {
    display_index: usize,
    out_dir: PathBuf,
    interval_ms: u64,
    /// Noneの場合は`--seconds`未指定=無期限に動き続ける(Ctrl+Cで停止する想定)。
    seconds: Option<u64>,
}

fn parse_args() -> Args {
    let mut display_index = 0usize;
    let mut out_dir = PathBuf::from("desktop-shot/output");
    let mut interval_ms = 1000u64;
    let mut seconds = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--display-index" => {
                if let Some(v) = args.next() {
                    display_index = v.parse().unwrap_or(0);
                }
            }
            "--out-dir" => {
                if let Some(v) = args.next() {
                    out_dir = PathBuf::from(v);
                }
            }
            "--interval-ms" => {
                if let Some(v) = args.next() {
                    interval_ms = v.parse().unwrap_or(1000);
                }
            }
            "--seconds" => {
                if let Some(v) = args.next() {
                    seconds = v.parse().ok();
                }
            }
            other => eprintln!("[desktop-shot] ignoring unknown argument: {other}"),
        }
    }

    Args { display_index, out_dir, interval_ms, seconds }
}

fn main() {
    let args = parse_args();
    println!(
        "[desktop-shot] display_index={} out_dir={:?} interval_ms={} seconds={:?} (None = run until Ctrl+C)",
        args.display_index, args.out_dir, args.interval_ms, args.seconds
    );

    if let Err(e) = std::fs::create_dir_all(&args.out_dir) {
        eprintln!("[desktop-shot] failed to create out_dir: {e}");
        std::process::exit(1);
    }

    if let Err(e) = run(&args) {
        eprintln!("[desktop-shot] fatal error: {e}");
        std::process::exit(1);
    }
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
        Err(e) => format!("unknown-{e:?}"),
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

fn save_bgra_as_png(bgra: &[u8], width: u32, height: u32, out_path: &PathBuf) -> Result<(), String> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
        dst[0] = src[2]; // R
        dst[1] = src[1]; // G
        dst[2] = src[0]; // B
        dst[3] = if src[3] == 0 { 255 } else { src[3] }; // A
    }

    let file = File::create(out_path).map_err(|e| format!("create {out_path:?}: {e}"))?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| format!("png header: {e}"))?;
    writer.write_image_data(&rgba).map_err(|e| format!("png data: {e}"))?;
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    use win_desktop_duplication::{
        co_init, set_process_dpi_awareness,
        devices::AdapterFactory,
        duplication::{DesktopDuplicationApi, DuplicationApiOptions},
        errors::DDApiError,
        tex_reader::TextureReader,
    };

    set_process_dpi_awareness();
    co_init();
    eprintln!("[desktop-shot] DPI awareness set, COM initialized");

    let initial_desktop = attach_input_desktop().map_err(|e| format!("attach_input_desktop: {e:?}"))?;
    println!("[desktop-shot] initial input desktop: {initial_desktop:?}");

    // display_managerと同じ列挙順(アダプターを跨いだ通し番号)でdisplay_indexを解決する。
    let (adapter, display, width, height) = {
        let factory = AdapterFactory::new();
        let mut counter = 0usize;
        let mut found = None;

        eprintln!("[desktop-shot] Enumerating adapters/displays...");
        'outer: for adapter in factory {
            for display in adapter.iter_displays() {
                eprintln!("[desktop-shot]   [{counter}] display=\"{}\"", display.name());
                if counter == args.display_index {
                    let mode = display
                        .get_current_display_mode()
                        .map_err(|e| format!("get_current_display_mode: {e:?}"))?;
                    eprintln!("[desktop-shot]   -> Selected: {}x{}", mode.width, mode.height);
                    found = Some((adapter, display, mode.width, mode.height));
                    break 'outer;
                }
                counter += 1;
            }
        }

        found.ok_or_else(|| format!("display index {} not found", args.display_index))?
    };

    eprintln!("[desktop-shot] Creating DesktopDuplicationApi...");
    let mut dupl = DesktopDuplicationApi::new(adapter.clone(), display.clone())
        .map_err(|e| format!("DesktopDuplicationApi::new: {e:?}"))?;
    dupl.configure(DuplicationApiOptions { skip_cursor: false });
    eprintln!("[desktop-shot] DesktopDuplicationApi ready. Capture size: {width}x{height}");

    let (dev, ctx) = dupl.get_device_and_ctx();
    let mut reader = TextureReader::new(dev, ctx);
    let mut bgra_buf: Vec<u8> = Vec::new();

    let start = Instant::now();
    let deadline = args.seconds.map(Duration::from_secs);
    let mut last_desktop_name = initial_desktop;
    let mut shot_count: u64 = 0;

    while deadline.is_none_or(|d| start.elapsed() < d) {
        let tick_start = Instant::now();

        let desktop_name = attach_input_desktop().unwrap_or_else(|e| {
            eprintln!("[desktop-shot] attach_input_desktop failed: {e:?}");
            last_desktop_name.clone()
        });
        if desktop_name != last_desktop_name {
            println!("[desktop-shot] desktop changed: {last_desktop_name:?} -> {desktop_name:?}");
            last_desktop_name = desktop_name.clone();
        }

        match dupl.acquire_next_frame_now() {
            Ok(tex) => {
                if let Err(e) = reader.get_data(&mut bgra_buf, &tex) {
                    eprintln!("[desktop-shot] TextureReader::get_data error: {e:?}");
                } else {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let file_name = format!("{}_{}.png", sanitize(&desktop_name), ts);
                    let out_path = args.out_dir.join(&file_name);

                    match save_bgra_as_png(&bgra_buf, width, height, &out_path) {
                        Ok(()) => {
                            shot_count += 1;
                            println!("[desktop-shot] saved {out_path:?} (desktop={desktop_name:?}, #{shot_count})");
                        }
                        Err(e) => eprintln!("[desktop-shot] failed to save frame: {e}"),
                    }
                }
            }
            Err(DDApiError::AccessLost) => {
                // acquire_next_frame_now内部でDXGIインスタンスは既に再生成 (reacquire_dup)
                // 済みなので、次のtickでそのまま再取得すればよい。ただし切り替え直後は
                // まだ新しいデスクトップの初回フレームが来ていないことが多いので、
                // 高速に空振りを繰り返さないよう軽くsleepしてから続行する。
                println!("[desktop-shot] AccessLost (desktop={desktop_name:?}), instance re-created, retrying...");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(DDApiError::AccessDenied) => {
                println!("[desktop-shot] AccessDenied (desktop={desktop_name:?}), instance re-created, retrying...");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(DDApiError::Unexpected(msg)) => {
                // ライブラリのドキュメント通り、Unexpectedはreacquireでは回復しないので
                // DesktopDuplicationApiインスタンス自体を作り直す。
                eprintln!("[desktop-shot] Unexpected error, rebuilding DesktopDuplicationApi: {msg}");
                match DesktopDuplicationApi::new(adapter.clone(), display.clone()) {
                    Ok(mut new_dupl) => {
                        new_dupl.configure(DuplicationApiOptions { skip_cursor: false });
                        let (dev, ctx) = new_dupl.get_device_and_ctx();
                        reader = TextureReader::new(dev, ctx);
                        dupl = new_dupl;
                        println!("[desktop-shot] DesktopDuplicationApi rebuilt successfully");
                    }
                    Err(e) => eprintln!("[desktop-shot] rebuild failed: {e:?}"),
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                eprintln!("[desktop-shot] acquire_next_frame_now failed: {e:?}");
            }
        }

        let elapsed = tick_start.elapsed();
        let interval = Duration::from_millis(args.interval_ms);
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    println!("[desktop-shot] done: total_shots={shot_count}");
    Ok(())
}
