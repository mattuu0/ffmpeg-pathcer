"""record_until_ctrlc.py

Records the desktop (via ddagrab) to a local .mp4 file for as long as you
want -- press Ctrl+C to stop -- so you can eyeball the resulting recording
yourself (does it look right through a real UAC prompt, does the cursor
look right, etc.), rather than only trusting speed/dup/drop numbers.

Runs under SYSTEM privileges via PAExec, same pattern as start_stream.py --
SYSTEM has no interactive window station of its own by default, so this is
a meaningfully different environment than a normal user session and is
worth checking on its own (this is what you asked to have covered:
"recording until Ctrl+C, run as SYSTEM, I'll check the file myself").

Usage (run as Administrator):
    python poc\\record_until_ctrlc.py
    python poc\\record_until_ctrlc.py --output C:\\temp\\my_recording.mp4
    python poc\\record_until_ctrlc.py --no-proxy   # baseline, genuine avfilter-12.dll

Press Ctrl+C to stop recording; the .mp4 is finalized (moov atom written)
before the script exits, so the file is always playable, not truncated.
"""

import argparse
import ctypes
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


class Paths:
    def __init__(self, script_dir: Path):
        self.script_dir = script_dir
        self.root = script_dir.parent.resolve()
        self.ffmpeg_dir = self.root / "ffmpeg-master-latest-win64-lgpl-shared"
        self.bin_dir = self.ffmpeg_dir / "bin"
        self.ffmpeg_exe = self.bin_dir / "ffmpeg.exe"
        self.paexec_exe = self.bin_dir / "paexec.exe"
        self.proxy_dll_src = self.root / "output" / "ddagrab_proxy.dll"

        self.dll = DllPaths(self.root)


def check_prerequisites(paths: Paths) -> None:
    missing = []
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if not paths.paexec_exe.exists():
        missing.append(f"paexec.exe not found: {paths.paexec_exe}")
    if not paths.proxy_dll_src.exists():
        missing.append(f"Proxy DLL not found: {paths.proxy_dll_src}")

    if missing:
        for m in missing:
            print(f"[ERROR] {m}")
        sys.exit(1)


def build_ffmpeg_args(output_path: Path) -> list[str]:
    return [
        "-hide_banner", "-y",
        "-f", "lavfi",
        "-i", "ddagrab=output_idx=0:framerate=60:video_size=1920x1080",
        "-c:v", "hevc_nvenc",
        "-preset", "p4",
        "-b:v", "20000k",
        "-maxrate", "35000k",
        "-bufsize", "35000k",
        str(output_path),
    ]


def run_system(paths: Paths, wrapper_cmd: Path, output_path: Path) -> subprocess.Popen:
    ffmpeg_args = build_ffmpeg_args(output_path)
    # No output redirection: PAExec -i opens ffmpeg in its own console window
    # on the interactive desktop, so live progress (frame=... fps=...) is
    # visible in real time there, same as running it by hand. Pressing 'q'
    # or Ctrl+C in THAT window is what actually reaches ffmpeg's stdin --
    # PAExec itself does not reliably forward this script's own Ctrl+C to
    # the remote (SYSTEM) process, which is why the finally block below also
    # force-kills ffmpeg.exe by name as a backstop.
    wrapper_cmd.write_text(
        "@echo off\r\n"
        f'"{paths.ffmpeg_exe}" {" ".join(ffmpeg_args)}\r\n',
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
    proxy_group = parser.add_mutually_exclusive_group()
    proxy_group.add_argument("--proxy", action="store_true", default=True, help="Use ddagrab_proxy.dll (default)")
    proxy_group.add_argument("--no-proxy", action="store_true", help="Use the genuine avfilter-12.dll instead")
    args = parser.parse_args()
    use_proxy = not args.no_proxy

    if not is_admin():
        print("[ERROR] This script must be run as Administrator (PAExec -s requires it).")
        return 1

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_prerequisites(paths)

    if args.output is not None:
        output_path = args.output.resolve()
    else:
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        results_dir = paths.root / "poc" / "verify-matrix" / "results"
        results_dir.mkdir(parents=True, exist_ok=True)
        output_path = results_dir / f"recording_{timestamp}.mp4"
    output_path.parent.mkdir(parents=True, exist_ok=True)

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    ffmpeg_proc: subprocess.Popen | None = None
    wrapper_cmd = script_dir / "run_ffmpeg_record.cmd"

    try:
        apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)

        proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
        proxy_log_path.unlink(missing_ok=True)

        print()
        print("=" * 60)
        print(f"[INFO] Mode: proxy={'on' if use_proxy else 'off'} privileges=SYSTEM")
        print(f"[INFO] Recording to: {output_path}")
        print("[INFO] Trigger UAC prompts / do whatever you want to test while this runs.")
        print("=" * 60)
        print()

        ffmpeg_proc = run_system(paths, wrapper_cmd, output_path)
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

    if output_path.exists():
        size_mb = output_path.stat().st_size / (1024 * 1024)
        print(f"[INFO] Recording saved: {output_path} ({size_mb:.1f} MB)")
    else:
        print(f"[WARN] Expected output file not found: {output_path}")
        print("       (SYSTEM's ffmpeg may have been killed before it could finalize the file --")
        print("       prefer stopping with 'q' in the ffmpeg console window over Ctrl+C when possible.)")

    proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
    if proxy_log_path.exists():
        print(f"[INFO] Proxy log: {proxy_log_path}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
