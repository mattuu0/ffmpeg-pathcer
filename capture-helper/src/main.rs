//! UACの同意プロンプト(セキュアデスクトップ)も含めて画面をキャプチャするための
//! 独立ヘルパープロセス。senderからPAExec経由でSYSTEM権限で起動されることを前提とする。
//!
//! DDA(Desktop Duplication API)はSYSTEM権限から起動した場合にセキュアデスクトップを
//! キャプチャできることが知られているが(https://github.com/robmikh/Win32CaptureSample/issues/48)、
//! それだけでは不十分で、キャプチャを行うスレッド自身がUACのセキュアデスクトップに
//! (OpenInputDesktop + SetThreadDesktop経由で)明示的にアタッチしている必要がある。
//! これを行わないと、SYSTEM権限であってもデフォルトの対話的デスクトップしか
//! キャプチャできない。
//!
//! このプロセス自身がffmpegを子プロセスとして起動し、キャプチャしたフレーム
//! (BGRA→I420変換後のrawvideo)を直接ffmpegの標準入力へパイプで書き込む
//! (senderは中継しない)。ffmpegのエンコード結果は、ffmpeg自身がTCP経由で
//! senderへ送る(ffmpeg起動引数はsenderから--ffmpeg-argとして渡される)。
//! これによりPAExec経由でこのプロセスをSYSTEM権限起動すれば、子プロセスの
//! ffmpegも自然にSYSTEM権限を継承する。

use std::io::Write;
use std::process::{Command, Stdio};

struct Args {
    display_index: usize,
    width: u32,
    height: u32,
    fps: u32,
    ffmpeg_path: String,
    ffmpeg_args: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut display_index = None;
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut ffmpeg_path = None;
    let mut ffmpeg_args = Vec::new();

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut next_value = || {
            iter.next()
                .ok_or_else(|| format!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--display-index" => display_index = Some(next_value()?.parse::<usize>().map_err(|e| e.to_string())?),
            "--width" => width = Some(next_value()?.parse::<u32>().map_err(|e| e.to_string())?),
            "--height" => height = Some(next_value()?.parse::<u32>().map_err(|e| e.to_string())?),
            "--fps" => fps = Some(next_value()?.parse::<u32>().map_err(|e| e.to_string())?),
            "--ffmpeg-path" => ffmpeg_path = Some(next_value()?),
            "--ffmpeg-arg" => ffmpeg_args.push(next_value()?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        display_index: display_index.ok_or("--display-index is required")?,
        width: width.ok_or("--width is required")?,
        height: height.ok_or("--height is required")?,
        fps: fps.ok_or("--fps is required")?,
        ffmpeg_path: ffmpeg_path.ok_or("--ffmpeg-path is required")?,
        ffmpeg_args,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[capture-helper] argument error: {e}");
            eprintln!(
                "usage: capture-helper --display-index <N> --width <W> --height <H> --fps <F> \
                 --ffmpeg-path <PATH> [--ffmpeg-arg <ARG>]..."
            );
            std::process::exit(2);
        }
    };

    eprintln!(
        "[capture-helper] starting: display_index={} {}x{} @{}fps, ffmpeg={}",
        args.display_index, args.width, args.height, args.fps, args.ffmpeg_path
    );
    eprintln!("[capture-helper] ffmpeg args: {}", args.ffmpeg_args.join(" "));

    let mut ffmpeg = match Command::new(&args.ffmpeg_path)
        .args(&args.ffmpeg_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[capture-helper] failed to spawn ffmpeg ({}): {e}", args.ffmpeg_path);
            std::process::exit(1);
        }
    };
    eprintln!("[capture-helper] ffmpeg spawned (pid={})", ffmpeg.id());

    let ffmpeg_stdin = ffmpeg.stdin.take().expect("ffmpeg stdin should be piped");

    if let Err(e) = capture_loop(&args, ffmpeg_stdin) {
        eprintln!("[capture-helper] fatal error: {e}");
        let _ = ffmpeg.kill();
        std::process::exit(1);
    }

    // キャプチャループが正常終了(通常はffmpeg側の切断によるstdin書き込み失敗)した場合、
    // ffmpegの終了も待ってからプロセスを終える。
    let _ = ffmpeg.wait();
}

/// 現在のスレッドをUAC同意プロンプト(セキュアデスクトップ)を含む「現在の入力デスクトップ」に
/// アタッチする。SYSTEM権限のプロセスから呼んで初めて意味を持つ(通常ユーザー権限では
/// Winlogon/セキュアデスクトップへのOpenDesktopが権限不足で失敗する)。
///
/// デスクトップは随時切り替わる(Default -> Winlogon -> Default等)ため、起動時に一度
/// 呼ぶだけでは不十分で、キャプチャループの毎tickでこれを呼び直し、その時点の入力
/// デスクトップに追従し続ける必要がある。追従できていないと、DDAが
/// AccessLost/AccessDenied後に内部で複製を再生成しても「アタッチしたままの古い
/// デスクトップ」に対して再生成してしまい、UAC後にキャプチャが更新されなくなる。
///
/// 失敗しても致命的エラーにはせず、直前のデスクトップ名を返してキャプチャを続行する
/// (通常デスクトップのキャプチャ自体はこれが無くても機能するため)。
fn attach_to_input_desktop(last_desktop_name: &str) -> String {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
        DESKTOP_ACCESS_FLAGS, DF_ALLOWOTHERACCOUNTHOOK, HDESK, UOI_NAME,
    };

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

    unsafe {
        let desktop = match OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, false, DESKTOP_ACCESS_FLAGS(0x01FF)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[capture-helper] OpenInputDesktop failed (continuing with {last_desktop_name:?}): {e:?}");
                return last_desktop_name.to_string();
            }
        };

        let name = desktop_name(desktop);
        let switch = SetThreadDesktop(desktop);
        let _ = CloseDesktop(desktop);

        if let Err(e) = switch {
            eprintln!("[capture-helper] SetThreadDesktop failed (continuing with {last_desktop_name:?}): {e:?}");
            last_desktop_name.to_string()
        } else {
            name
        }
    }
}

fn capture_loop(args: &Args, mut ffmpeg_stdin: std::process::ChildStdin) -> Result<(), String> {
    use win_desktop_duplication::{
        co_init, set_process_dpi_awareness,
        devices::AdapterFactory,
        duplication::{DesktopDuplicationApi, DuplicationApiOptions},
        errors::DDApiError,
        tex_reader::TextureReader,
    };

    eprintln!(
        "[capture-helper] capture_loop started: display_index={} fps={} target_size={}x{}",
        args.display_index, args.fps, args.width, args.height
    );

    set_process_dpi_awareness();
    co_init();
    eprintln!("[capture-helper] DPI awareness set, COM initialized");

    // display_managerと同じ列挙順(アダプターを跨いだ通し番号)でdisplay_indexを解決する。
    let (adapter, display, width, height, disp_left, disp_top) = {
        let factory = AdapterFactory::new();
        let mut counter = 0usize;
        let mut found = None;

        eprintln!("[capture-helper] Enumerating adapters/displays...");
        'outer: for adapter in factory {
            for display in adapter.iter_displays() {
                eprintln!("[capture-helper]   [{counter}] display=\"{}\"", display.name());
                if counter == args.display_index {
                    let mode = display
                        .get_current_display_mode()
                        .map_err(|e| format!("get_current_display_mode: {e:?}"))?;

                    let (left, top) = display_desktop_origin(&display.name());
                    eprintln!(
                        "[capture-helper]   -> Selected: {}x{} origin=({left},{top})",
                        mode.width, mode.height
                    );

                    found = Some((adapter, display, mode.width, mode.height, left, top));
                    break 'outer;
                }
                counter += 1;
            }
        }

        found.ok_or_else(|| format!("display index {} not found", args.display_index))?
    };

    eprintln!("[capture-helper] Creating DesktopDuplicationApi...");
    let mut dupl = DesktopDuplicationApi::new(adapter.clone(), display.clone())
        .map_err(|e| format!("DesktopDuplicationApi::new: {e:?}"))?;
    dupl.configure(DuplicationApiOptions { skip_cursor: true });
    eprintln!("[capture-helper] DesktopDuplicationApi ready. Capture size: {width}x{height}");

    let (dev, ctx) = dupl.get_device_and_ctx();
    let mut reader = TextureReader::new(dev, ctx);
    let mut cursor_state = CursorState::default();
    let mut last_desktop_name = attach_to_input_desktop("Default");
    eprintln!("[capture-helper] initial input desktop: {last_desktop_name:?}");

    let start_time = std::time::Instant::now();
    let frame_interval = std::time::Duration::from_secs_f64(1.0 / args.fps as f64);
    let mut next_frame_time = start_time;
    let mut bgra_buf: Vec<u8> = Vec::new();

    let mut frame_count = 0u64;
    let mut access_lost_count = 0u64;
    let mut last_stat_time = start_time;
    let mut acquire_time_total = std::time::Duration::ZERO;
    let mut acquire_time_max = std::time::Duration::ZERO;
    let mut convert_time_total = std::time::Duration::ZERO;
    let mut convert_time_max = std::time::Duration::ZERO;
    // ffmpeg stdinへのwrite_all所要時間。ここが大きい/ばらつく場合はffmpeg(NVENC)側の
    // 処理が追いついておらずstdinパイプにバックプレッシャーがかかっていることを示す。
    let mut write_time_total = std::time::Duration::ZERO;
    let mut write_time_max = std::time::Duration::ZERO;
    let mut stat_frame_count = 0u64;

    eprintln!("[capture-helper] Entering capture loop");

    loop {
        // デスクトップは随時切り替わる(Default <-> Winlogon/UAC同意画面/Screen-saver)ため、
        // 毎tickこのスレッドを現在の入力デスクトップへ再アタッチする。これを怠ると、
        // secure desktop側に切り替わった後もこのスレッドはDefaultに固定されたままになり、
        // DDAがAccessLost後に複製を再生成しても古いデスクトップに対して再生成してしまい、
        // UAC表示中〜終了後にキャプチャが更新されなくなる。
        let desktop_name = attach_to_input_desktop(&last_desktop_name);
        if desktop_name != last_desktop_name {
            eprintln!("[capture-helper] desktop changed: {last_desktop_name:?} -> {desktop_name:?}");
            last_desktop_name = desktop_name;
        }

        let acquire_start = std::time::Instant::now();
        let tex = match dupl.acquire_next_frame_now() {
            Ok(t) => t,
            Err(DDApiError::AccessLost) => {
                access_lost_count += 1;
                if access_lost_count <= 3 || access_lost_count % 100 == 0 {
                    eprintln!(
                        "[capture-helper] AccessLost #{access_lost_count} (desktop={last_desktop_name:?}), instance re-created, retrying..."
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(DDApiError::AccessDenied) => {
                eprintln!(
                    "[capture-helper] AccessDenied (desktop={last_desktop_name:?}), instance re-created, retrying..."
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(DDApiError::Unexpected(msg)) => {
                // ライブラリのドキュメント通り、Unexpectedはreacquireでは回復しないので
                // DesktopDuplicationApiインスタンス自体を作り直す。ここで即座に終了して
                // しまうと、以後ずっとキャプチャが停止したffmpegプロセスだけが残ってしまう。
                eprintln!("[capture-helper] Unexpected error, rebuilding DesktopDuplicationApi: {msg}");
                match DesktopDuplicationApi::new(adapter.clone(), display.clone()) {
                    Ok(mut new_dupl) => {
                        new_dupl.configure(DuplicationApiOptions { skip_cursor: true });
                        let (dev, ctx) = new_dupl.get_device_and_ctx();
                        reader = TextureReader::new(dev, ctx);
                        dupl = new_dupl;
                        eprintln!("[capture-helper] DesktopDuplicationApi rebuilt successfully");
                    }
                    Err(e) => eprintln!("[capture-helper] rebuild failed: {e:?}"),
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            Err(e) => {
                eprintln!("[capture-helper] acquire_next_frame_now failed (desktop={last_desktop_name:?}): {e:?}, retrying...");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };

        if let Err(e) = reader.get_data(&mut bgra_buf, &tex) {
            eprintln!("[capture-helper] TextureReader::get_data error: {e:?}");
            return Err(format!("TextureReader::get_data: {e:?}"));
        }
        let acquire_elapsed = acquire_start.elapsed();

        if frame_count == 0 {
            eprintln!(
                "[capture-helper] First frame captured! bgra_buf.len()={} expected={}",
                bgra_buf.len(),
                width * height * 4
            );
        }

        draw_cursor_bgra(&mut bgra_buf, width, height, disp_left, disp_top, &mut cursor_state);

        let convert_start = std::time::Instant::now();
        let yuv = bgra_to_i420(&bgra_buf, width, height);
        let convert_elapsed = convert_start.elapsed();

        acquire_time_total += acquire_elapsed;
        acquire_time_max = acquire_time_max.max(acquire_elapsed);
        convert_time_total += convert_elapsed;
        convert_time_max = convert_time_max.max(convert_elapsed);
        stat_frame_count += 1;

        // ffmpegの標準入力への書き込み。ffmpegが終了/パイプを閉じた場合は
        // エラーになるのでループを抜けてプロセスを終了する。
        let write_start = std::time::Instant::now();
        let write_result = ffmpeg_stdin.write_all(&yuv);
        let write_elapsed = write_start.elapsed();
        if let Err(e) = write_result {
            eprintln!("[capture-helper] ffmpeg stdin write failed, stopping (ffmpeg likely exited): {e}");
            break;
        }
        write_time_total += write_elapsed;
        write_time_max = write_time_max.max(write_elapsed);
        frame_count += 1;
        if frame_count <= 5 {
            eprintln!("[capture-helper] Frame #{frame_count} sent");
        }

        let elapsed = last_stat_time.elapsed();
        if elapsed >= std::time::Duration::from_secs(5) {
            let fps_actual = frame_count as f64 / start_time.elapsed().as_secs_f64();
            let acquire_avg_ms = if stat_frame_count > 0 {
                acquire_time_total.as_secs_f64() * 1000.0 / stat_frame_count as f64
            } else {
                0.0
            };
            let convert_avg_ms = if stat_frame_count > 0 {
                convert_time_total.as_secs_f64() * 1000.0 / stat_frame_count as f64
            } else {
                0.0
            };
            let write_avg_ms = if stat_frame_count > 0 {
                write_time_total.as_secs_f64() * 1000.0 / stat_frame_count as f64
            } else {
                0.0
            };
            eprintln!(
                "[capture-helper] Stats: total_frames={frame_count} access_lost={access_lost_count} actual_fps={fps_actual:.1} \
                 acquire_avg={acquire_avg_ms:.2}ms acquire_max={:.2}ms convert_avg={convert_avg_ms:.2}ms convert_max={:.2}ms \
                 ffmpeg_write_avg={write_avg_ms:.2}ms ffmpeg_write_max={:.2}ms",
                acquire_time_max.as_secs_f64() * 1000.0,
                convert_time_max.as_secs_f64() * 1000.0,
                write_time_max.as_secs_f64() * 1000.0,
            );
            last_stat_time = std::time::Instant::now();
            acquire_time_total = std::time::Duration::ZERO;
            acquire_time_max = std::time::Duration::ZERO;
            convert_time_total = std::time::Duration::ZERO;
            convert_time_max = std::time::Duration::ZERO;
            write_time_total = std::time::Duration::ZERO;
            write_time_max = std::time::Duration::ZERO;
            stat_frame_count = 0;
        }

        next_frame_time += frame_interval;
        let now = std::time::Instant::now();
        if now < next_frame_time {
            std::thread::sleep(next_frame_time - now);
        } else {
            next_frame_time = now;
        }
    }

    eprintln!("[capture-helper] Loop exited. total_frames={frame_count}");
    Ok(())
}

/// Query the desktop origin (left, top in virtual-screen coordinates) for a named GDI device.
fn display_desktop_origin(device_name: &str) -> (i32, i32) {
    use std::ffi::CString;
    use windows::core::PCSTR;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsExA, DEVMODEA, DM_POSITION, ENUM_CURRENT_SETTINGS,
        ENUM_DISPLAY_SETTINGS_FLAGS,
    };

    let name_c = match CString::new(device_name) {
        Ok(s) => s,
        Err(_) => return (0, 0),
    };
    let mut dm = DEVMODEA {
        dmSize: std::mem::size_of::<DEVMODEA>() as u16,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplaySettingsExA(
            PCSTR(name_c.as_ptr() as *const u8),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
        .as_bool()
    };
    if ok && (dm.dmFields & DM_POSITION).0 != 0 {
        unsafe {
            (
                dm.Anonymous1.Anonymous2.dmPosition.x,
                dm.Anonymous1.Anonymous2.dmPosition.y,
            )
        }
    } else {
        (0, 0)
    }
}

/// Cached cursor bitmap to avoid re-fetching every frame when the cursor hasn't changed.
#[derive(Default)]
struct CursorState {
    handle: usize,
    hotspot_x: i32,
    hotspot_y: i32,
    pixels: Vec<u8>,
    cursor_w: u32,
    cursor_h: u32,
}

fn draw_cursor_bgra(
    bgra: &mut [u8],
    frame_w: u32,
    frame_h: u32,
    disp_left: i32,
    disp_top: i32,
    state: &mut CursorState,
) {
    use std::mem::size_of;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, ICONINFO,
    };

    let mut ci = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut ci) }.is_err() {
        return;
    }
    if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
        return;
    }

    let cursor_raw = ci.hCursor.0 as usize;

    if cursor_raw != state.handle {
        let mut ii = ICONINFO::default();
        if unsafe { GetIconInfo(ci.hCursor.into(), &mut ii) }.is_err() {
            return;
        }

        state.hotspot_x = ii.xHotspot as i32;
        state.hotspot_y = ii.yHotspot as i32;
        state.handle = cursor_raw;
        state.pixels.clear();

        let hbm = if !ii.hbmColor.is_invalid() {
            ii.hbmColor
        } else {
            ii.hbmMask
        };

        let mut bm = BITMAP::default();
        if unsafe { GetObjectW(hbm.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) } == 0 {
            unsafe {
                if !ii.hbmColor.is_invalid() {
                    let _ = DeleteObject(ii.hbmColor.into());
                }
            }
            unsafe {
                if !ii.hbmMask.is_invalid() {
                    let _ = DeleteObject(ii.hbmMask.into());
                }
            }
            return;
        }

        let cw = bm.bmWidth as u32;
        let ch = if ii.hbmColor.is_invalid() {
            (bm.bmHeight as u32) / 2
        } else {
            bm.bmHeight as u32
        };

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: cw as i32,
                biHeight: -(ch as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pixels = vec![0u8; (cw * ch * 4) as usize];
        let hdc = unsafe { CreateCompatibleDC(None) };
        let got = unsafe {
            GetDIBits(
                hdc,
                hbm,
                0,
                ch,
                Some(pixels.as_mut_ptr() as *mut _),
                &bmi as *const _ as *mut _,
                DIB_RGB_COLORS,
            )
        };
        unsafe {
            let _ = DeleteDC(hdc);
        }
        unsafe {
            if !ii.hbmColor.is_invalid() {
                let _ = DeleteObject(ii.hbmColor.into());
            }
        }
        unsafe {
            if !ii.hbmMask.is_invalid() {
                let _ = DeleteObject(ii.hbmMask.into());
            }
        }

        if got == 0 {
            return;
        }

        state.pixels = pixels;
        state.cursor_w = cw;
        state.cursor_h = ch;
    }

    if state.pixels.is_empty() {
        return;
    }

    let cx = ci.ptScreenPos.x - disp_left - state.hotspot_x;
    let cy = ci.ptScreenPos.y - disp_top - state.hotspot_y;

    let cw = state.cursor_w as i32;
    let ch = state.cursor_h as i32;
    let fw = frame_w as i32;
    let fh = frame_h as i32;

    // 画面外にはみ出す範囲を事前にクリップしておくことで、ループ内側から
    // 境界チェック(if fy/fx < 0 || >= fh/fw)を排除する。カーソルは通常
    // ほぼ全体が画面内に収まっているため、この分岐削除だけでもホットパスが
    // 軽くなる。
    let py_start = (-cy).max(0);
    let py_end = (fh - cy).min(ch);
    let px_start = (-cx).max(0);
    let px_end = (fw - cx).min(cw);
    if py_start >= py_end || px_start >= px_end {
        return;
    }

    for py in py_start..py_end {
        let fy = cy + py;
        let src_row_base = (py * cw) as usize;
        let dst_row_base = (fy * fw) as usize;
        for px in px_start..px_end {
            let fx = (cx + px) as usize;
            let src = (src_row_base + px as usize) * 4;
            let dst = (dst_row_base + fx) * 4;

            // クリップ済みなので通常は範囲内だが、フレームサイズとカーソル
            // バッファの取得タイミングがずれた場合の保険として残す。
            if dst + 3 >= bgra.len() || src + 3 >= state.pixels.len() {
                continue;
            }

            let a = state.pixels[src + 3];
            if a == 0 {
                continue;
            }
            if a == 255 {
                // 完全不透明: アルファブレンド計算を省略して単純コピーする
                // (カラーカーソルの大半はマスクベースでa=0かa=255の二値のため、
                // このパスがほぼ常に使われる)。
                bgra[dst] = state.pixels[src];
                bgra[dst + 1] = state.pixels[src + 1];
                bgra[dst + 2] = state.pixels[src + 2];
                continue;
            }

            let a = a as u32;
            for ch_i in 0..3 {
                let s = state.pixels[src + ch_i] as u32;
                let d = bgra[dst + ch_i] as u32;
                bgra[dst + ch_i] = ((s * a + d * (255 - a)) / 255) as u8;
            }
        }
    }
}

fn bgra_to_i420(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    use rayon::prelude::*;

    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_w = w / 2;
    let uv_h = h / 2;
    let uv_size = uv_w * uv_h;
    let mut yuv = vec![0u8; y_size + 2 * uv_size];
    let (y_plane, uv_planes) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_size);

    // BT.601 full-range coefficients (Y: 0-255, UV: 0-255 centred at 128).
    y_plane.par_chunks_mut(w).enumerate().for_each(|(row, y_row)| {
        let src_row = &bgra[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let i = col * 4;
            let b = src_row[i] as i32;
            let g = src_row[i + 1] as i32;
            let r = src_row[i + 2] as i32;
            y_row[col] = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255) as u8;
        }
    });

    u_plane
        .par_chunks_mut(uv_w)
        .zip(v_plane.par_chunks_mut(uv_w))
        .enumerate()
        .for_each(|(uv_row, (u_row, v_row))| {
            for uv_col in 0..uv_w {
                let mut sum_u: i32 = 0;
                let mut sum_v: i32 = 0;
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let row = uv_row * 2 + dy;
                        let col = uv_col * 2 + dx;
                        let i = (row * w + col) * 4;
                        let b = bgra[i] as i32;
                        let g = bgra[i + 1] as i32;
                        let r = bgra[i + 2] as i32;
                        sum_u += (-43 * r - 85 * g + 128 * b) >> 8;
                        sum_v += (128 * r - 107 * g - 21 * b) >> 8;
                    }
                }
                u_row[uv_col] = ((sum_u / 4) + 128).clamp(0, 255) as u8;
                v_row[uv_col] = ((sum_v / 4) + 128).clamp(0, 255) as u8;
            }
        });

    yuv
}
