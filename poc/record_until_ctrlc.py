"""record_until_ctrlc.py

Adds a Parsec Virtual Display Driver (VDD) display, records THAT display
(via ddagrab) to a local .mp4 file for as long as you want -- press Ctrl+C
to stop -- so you can eyeball the resulting recording yourself, rather than
only trusting speed/dup/drop numbers. Also writes ffmpeg's own level=48
(AV_LOG_DEBUG) log via FFREPORT, matching the log style already used
elsewhere in this project. The virtual display is always removed again on
exit (Ctrl+C, ffmpeg exiting on its own, or an error), even if something
goes wrong partway through.

Self-elevates to Administrator if not already running as one (via
ShellExecuteW "runas"), then runs ffmpeg under SYSTEM privileges via PAExec
-- same two-step pattern as start_stream.py/verify_matrix.py. This script
itself never triggers a UAC prompt/desktop switch on its own; if you want to
test recovery across a UAC-style desktop switch, trigger that yourself
(e.g. an actual UAC prompt, Win+L, etc.) while this is recording.

Usage:
    python poc\\record_until_ctrlc.py
    python poc\\record_until_ctrlc.py --output C:\\temp\\my_recording.mp4
    python poc\\record_until_ctrlc.py --output-idx 0    # capture the primary display instead of adding a VDD
    python poc\\record_until_ctrlc.py --no-proxy        # baseline, genuine avfilter-12.dll

Press Ctrl+C to stop recording; the .mp4 is finalized (moov atom written)
before the script exits, so the file is always playable, not truncated.
"""

import argparse
import ctypes
import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from dll_layout import BrokenDllLayoutError, DllPaths, apply_dll_mode, restore_dll_layout, stash_dll_layout


def is_admin() -> bool:
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def relaunch_as_admin() -> None:
    """Re-invokes this exact script (same interpreter, same argv) elevated,
    via ShellExecuteW's "runas" verb -- this itself triggers the ONE UAC
    prompt needed to get an elevated process at all, same as double-clicking
    a shortcut marked "Run as administrator" would. The original,
    non-elevated process just waits for the elevated child logically (by
    exiting once the relaunch is confirmed to have started); output goes to
    the new elevated console window, not back to this one.
    """
    params = " ".join(f'"{arg}"' for arg in sys.argv)
    ret = ctypes.windll.shell32.ShellExecuteW(None, "runas", sys.executable, params, None, 1)
    if ret <= 32:
        print(f"[ERROR] Failed to relaunch elevated (ShellExecuteW returned {ret}).")
        sys.exit(1)


class Paths:
    def __init__(self, script_dir: Path):
        self.script_dir = script_dir
        self.root = script_dir.parent.resolve()
        self.ffmpeg_dir = self.root / "ffmpeg-master-latest-win64-lgpl-shared"
        self.bin_dir = self.ffmpeg_dir / "bin"
        self.ffmpeg_exe = self.bin_dir / "ffmpeg.exe"
        self.paexec_exe = self.bin_dir / "paexec.exe"
        self.proxy_dll_src = self.root / "output" / "ddagrab_proxy.dll"
        self.vdd_helper_exe = self.root / "target" / "release" / "vdd-helper.exe"

        self.dll = DllPaths(self.root)


def check_prerequisites(paths: Paths, need_vdd: bool) -> None:
    missing = []
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if not paths.paexec_exe.exists():
        missing.append(f"paexec.exe not found: {paths.paexec_exe}")
    if not paths.proxy_dll_src.exists():
        missing.append(f"Proxy DLL not found: {paths.proxy_dll_src}")
    if need_vdd and not paths.vdd_helper_exe.exists():
        missing.append(
            f"vdd-helper.exe not found: {paths.vdd_helper_exe}\n"
            "        Build it first with: cargo build --release -p vdd-helper"
        )

    if missing:
        for m in missing:
            print(f"[ERROR] {m}")
        sys.exit(1)


def add_virtual_display(paths: Paths) -> tuple[subprocess.Popen, int, int]:
    """Starts vdd-helper (which stays running -- see its own module doc
    comment for why: the VDD driver needs a live client calling
    vdd_update() periodically or the display it added disappears again
    almost immediately). Returns (process, monitor_id, output_idx); the
    caller must keep `process` alive for as long as it wants the display to
    exist, then call remove_virtual_display(process, ...) to tear it down.
    """
    proc = subprocess.Popen(
        [str(paths.vdd_helper_exe)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1,
    )

    first_line = proc.stdout.readline()
    match = re.search(r"monitor_id=(\d+) output_idx=(\d+)", first_line)
    if not match:
        stderr_output = proc.stderr.read()
        proc.kill()
        proc.wait(timeout=5)
        raise RuntimeError(
            f"vdd-helper did not report success on its first line "
            f"(got {first_line!r}); stderr: {stderr_output.strip()}"
        )
    return proc, int(match.group(1)), int(match.group(2))


def remove_virtual_display(proc: subprocess.Popen, monitor_id: int) -> None:
    """Asks the still-running vdd-helper process (from add_virtual_display)
    to remove the display and exit cleanly. Falls back to killing it if it
    doesn't exit promptly -- the VDD driver is expected to tear the display
    down on its own once the handle closes either way, so this is a
    best-effort nicety, not the only cleanup path.
    """
    try:
        if proc.stdin is not None:
            proc.stdin.write("remove\n")
            proc.stdin.flush()
        proc.wait(timeout=5)
    except Exception as e:
        print(f"[WARN] vdd-helper did not exit cleanly after 'remove' (monitor_id={monitor_id}): {e}")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


def build_ffmpeg_args(output_idx: int, output_path: Path) -> list[str]:
    # Same low-latency NVENC tuning as the user's own working wrapper
    # (rust-castsender's sender), just writing to a local .mp4 file instead
    # of streaming over TCP -- everything else (preset/tune/rc/gop
    # structure) is unchanged so this exercises the exact same encoder path.
    return [
        "-f", "lavfi",
        "-i", f"ddagrab=output_idx={output_idx}:framerate=60:video_size=1920x1080",
        "-c:v", "hevc_nvenc",
        "-preset", "p1",
        "-tune", "ull",
        "-profile:v", "main",
        "-g", "30",
        "-bf", "0",
        "-forced-idr", "1",
        "-rc-lookahead", "0",
        "-delay", "0",
        "-zerolatency", "1",
        "-rc", "vbr",
        "-b:v", "8000k",
        "-minrate", "8000k",
        "-maxrate", "35000k",
        "-bufsize", "35000k",
        "-intra-refresh", "1",
        "-force_key_frames", "expr:gte(t,n_forced*0.5)",
        "-y",
        str(output_path),
    ]


def run_system(paths: Paths, wrapper_cmd: Path, output_idx: int, output_path: Path, ffmpeg_log_path: Path) -> subprocess.Popen:
    ffmpeg_args = build_ffmpeg_args(output_idx, output_path)
    # FFREPORT's value is colon-separated (file=<path>:level=<n>), which
    # collides with the drive-letter colon in an absolute Windows path --
    # escape it the same way the user's own wrapper does (`C\:\Users\...`),
    # rather than switching to a relative path/cwd trick.
    escaped_log_path = str(ffmpeg_log_path).replace(":", "\\:")

    # No output redirection: PAExec -i opens ffmpeg in its own console window
    # on the interactive desktop, so live progress is visible in real time
    # there, same as running it by hand. Pressing 'q' or Ctrl+C in THAT
    # window is what actually reaches ffmpeg's stdin -- PAExec itself does
    # not reliably forward this script's own Ctrl+C to the remote (SYSTEM)
    # process, which is why the finally block below also force-kills
    # ffmpeg.exe by name as a backstop.
    args_str = " ".join(f'"{a}"' for a in ffmpeg_args)
    wrapper_cmd.write_text(
        "@echo off\r\n"
        f"set FFREPORT=file={escaped_log_path}:level=48\r\n"
        f'"{paths.ffmpeg_exe}" {args_str}\r\n',
        encoding="ascii",
    )
    print("[INFO] Starting ffmpeg with SYSTEM privileges (via PAExec) -- watch the new console window for progress")
    print("[INFO] Press 'q' (or Ctrl+C) in THAT console window to stop recording gracefully.")
    return subprocess.Popen(
        [str(paths.paexec_exe), "-s", "-i", "-w", str(paths.bin_dir), str(wrapper_cmd)],
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--output", type=Path, default=None,
        help="Output .mp4 path (default: poc\\verify-matrix\\results\\recording_<timestamp>.mp4)",
    )
    parser.add_argument(
        "--output-idx", type=int, default=None,
        help="ddagrab output_idx to capture. Default: automatically add a fresh Parsec VDD "
             "virtual display and capture THAT (removed again on exit). Pass an explicit "
             "index (e.g. 0 for the primary display) to skip adding a VDD and capture an "
             "existing display instead.",
    )
    proxy_group = parser.add_mutually_exclusive_group()
    proxy_group.add_argument("--proxy", action="store_true", default=True, help="Use ddagrab_proxy.dll (default)")
    proxy_group.add_argument("--no-proxy", action="store_true", help="Use the genuine avfilter-12.dll instead")
    args = parser.parse_args()
    use_proxy = not args.no_proxy

    if not is_admin():
        print("[INFO] Not running as Administrator -- requesting elevation (one UAC prompt)...")
        relaunch_as_admin()
        return 0

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    need_vdd = args.output_idx is None
    check_prerequisites(paths, need_vdd)

    if args.output is not None:
        output_path = args.output.resolve()
    else:
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        results_dir = paths.root / "poc" / "verify-matrix" / "results"
        results_dir.mkdir(parents=True, exist_ok=True)
        output_path = results_dir / f"recording_{timestamp}.mp4"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    ffmpeg_log_path = output_path.with_suffix("").with_suffix(".ffmpeg.log")

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    ffmpeg_proc: subprocess.Popen | None = None
    wrapper_cmd = script_dir / "run_ffmpeg_record.cmd"
    vdd_proc: subprocess.Popen | None = None
    vdd_monitor_id: int | None = None

    try:
        apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)

        proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
        proxy_log_path.unlink(missing_ok=True)

        if need_vdd:
            print("[INFO] Adding a Parsec VDD virtual display...")
            vdd_proc, vdd_monitor_id, output_idx = add_virtual_display(paths)
            print(f"[INFO] Virtual display added: monitor_id={vdd_monitor_id} output_idx={output_idx}")
        else:
            output_idx = args.output_idx

        print()
        print("=" * 60)
        print(f"[INFO] Mode: proxy={'on' if use_proxy else 'off'} privileges=SYSTEM output_idx={output_idx}")
        print(f"[INFO] Recording to: {output_path}")
        print(f"[INFO] ffmpeg debug log (level=48): {ffmpeg_log_path}")
        print("[INFO] This script will not trigger anything itself -- do whatever you want to test")
        print("       (UAC prompts, Win+L, desktop switches, etc.) while it's recording.")
        print("=" * 60)
        print()

        ffmpeg_proc = run_system(paths, wrapper_cmd, output_idx, output_path, ffmpeg_log_path)
        return_code = ffmpeg_proc.wait()
        print(f"[INFO] ffmpeg exited with code {return_code}.")
    except KeyboardInterrupt:
        print("\n[INFO] Ctrl+C received here -- stopping recording...")
        if ffmpeg_proc is not None and ffmpeg_proc.poll() is None:
            ffmpeg_proc.terminate()
            try:
                ffmpeg_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                ffmpeg_proc.kill()
        # PAExec doesn't reliably forward Ctrl+C to the remote (SYSTEM)
        # ffmpeg process -- without this, ffmpeg (and thus the recording)
        # would keep running detached even after this script exits.
        subprocess.run(
            ["taskkill", "/f", "/im", "ffmpeg.exe"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    finally:
        wrapper_cmd.unlink(missing_ok=True)
        restore_dll_layout(paths.dll, stash_dir)
        if vdd_proc is not None:
            print(f"[INFO] Removing virtual display (monitor_id={vdd_monitor_id})...")
            remove_virtual_display(vdd_proc, vdd_monitor_id)

    if output_path.exists():
        size_mb = output_path.stat().st_size / (1024 * 1024)
        print(f"[INFO] Recording saved: {output_path} ({size_mb:.1f} MB)")
    else:
        print(f"[WARN] Expected output file not found: {output_path}")
        print("       (SYSTEM's ffmpeg may have been killed before it could finalize the file --")
        print("       prefer stopping with 'q' in the ffmpeg console window over Ctrl+C when possible.)")

    if ffmpeg_log_path.exists():
        print(f"[INFO] ffmpeg debug log: {ffmpeg_log_path}")

    proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
    if proxy_log_path.exists():
        print(f"[INFO] Proxy log: {proxy_log_path}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
