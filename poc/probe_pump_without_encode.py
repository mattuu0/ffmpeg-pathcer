"""probe_pump_without_encode.py

One-off diagnostic: runs ffmpeg with the proxy DLL active but WITHOUT any
video encoder or network output (`-f null -`), to isolate whether the ~41ms
AcquireNextFrame stalls seen in verify_matrix.py's proxy_normal/proxy_system
cases are caused by GPU contention with hevc_nvenc, or are inherent to the
pump thread's polling design regardless of what else is using the GPU.

Only runs the "proxy, normal privileges" case (the simplest one to reproduce
the stall in) -- this is a targeted probe, not a full matrix.

Usage: run as Administrator:
    python poc\\probe_pump_without_encode.py
"""

import ctypes
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

DURATION_SECS = 30


def is_admin() -> bool:
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def main() -> int:
    if not is_admin():
        print("[ERROR] This script must be run as Administrator.")
        return 1

    script_dir = Path(__file__).resolve().parent
    root = script_dir.parent
    ffmpeg_dir = root / "ffmpeg-master-latest-win64-lgpl-shared"
    bin_dir = ffmpeg_dir / "bin"
    ffmpeg_exe = bin_dir / "ffmpeg.exe"
    proxy_dll_src = root / "output" / "ddagrab_proxy.dll"

    real_dll = bin_dir / "avfilter-12.dll"
    orig_dll = bin_dir / "avfilter-12_orig.dll"

    if not ffmpeg_exe.exists():
        print(f"[ERROR] ffmpeg.exe not found: {ffmpeg_exe}")
        return 1
    if not proxy_dll_src.exists():
        print(f"[ERROR] Proxy DLL not found: {proxy_dll_src}")
        return 1

    # --- stash current DLL layout, same approach as verify_matrix.py -------
    stash_dir = Path(tempfile.mkdtemp(prefix="ddagrab_probe_stash_"))
    print(f"[INFO] Stashing current DLL layout -> {stash_dir}")
    if real_dll.exists():
        shutil.copy2(real_dll, stash_dir / "avfilter-12.dll")
    if orig_dll.exists():
        shutil.copy2(orig_dll, stash_dir / "avfilter-12_orig.dll")

    genuine_dll = stash_dir / "avfilter-12_genuine.dll"
    if (stash_dir / "avfilter-12_orig.dll").exists():
        shutil.copy2(stash_dir / "avfilter-12_orig.dll", genuine_dll)
    else:
        shutil.copy2(stash_dir / "avfilter-12.dll", genuine_dll)

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    results_dir = script_dir / "verify-matrix" / "results" / f"probe_noencode_{ts}"
    results_dir.mkdir(parents=True, exist_ok=True)
    print(f"[INFO] Results will be saved to: {results_dir}")

    try:
        print("[INFO] Switching avfilter-12.dll to the proxy build")
        real_dll.unlink(missing_ok=True)
        orig_dll.unlink(missing_ok=True)
        shutil.copy2(genuine_dll, orig_dll)
        shutil.copy2(proxy_dll_src, real_dll)

        proxy_log_path = bin_dir / "ddagrab_proxy.log"
        proxy_log_path.unlink(missing_ok=True)

        ffmpeg_log = results_dir / "ffmpeg.log"
        ffmpeg_args = [
            "-hide_banner", "-y",
            "-f", "lavfi",
            "-i", "ddagrab=output_idx=0:framerate=60:video_size=1920x1080",
            "-t", str(DURATION_SECS),
            "-f", "null", "-",
        ]

        print(f"[INFO] Running ffmpeg with -f null - (no encoder, no network output) for {DURATION_SECS}s")
        with ffmpeg_log.open("w", encoding="utf-8", errors="replace") as log_f:
            subprocess.run(
                [str(ffmpeg_exe), *ffmpeg_args],
                cwd=str(bin_dir),
                stdout=log_f,
                stderr=subprocess.STDOUT,
            )

        print(f"[INFO] ffmpeg finished. Log: {ffmpeg_log}")

        if proxy_log_path.exists():
            shutil.copy2(proxy_log_path, results_dir / "ddagrab_proxy.log")
            print(f"[INFO] Proxy log saved to: {results_dir / 'ddagrab_proxy.log'}")

    finally:
        print("[INFO] Restoring DLL layout from stash")
        real_dll.unlink(missing_ok=True)
        orig_dll.unlink(missing_ok=True)
        if (stash_dir / "avfilter-12.dll").exists():
            shutil.copy2(stash_dir / "avfilter-12.dll", real_dll)
        if (stash_dir / "avfilter-12_orig.dll").exists():
            shutil.copy2(stash_dir / "avfilter-12_orig.dll", orig_dll)
        shutil.rmtree(stash_dir, ignore_errors=True)

    print()
    print(f"[INFO] Done. Check {results_dir / 'ddagrab_proxy.log'} for [stats/1s] lines.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
