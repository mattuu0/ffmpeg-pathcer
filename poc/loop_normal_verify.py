"""loop_normal_verify.py

Runs the --normal-privilege-only half of verify_matrix.py's comparison
(proxy_normal vs noproxy_normal) repeatedly, without requiring Administrator
privileges or PAExec -- unlike verify_matrix.py, this never touches SYSTEM
execution, and swapping bin/avfilter-12.dll only needs write access to that
folder (not elevation) as long as no other process (ffmpeg, PAExec) has it
open.

Intended for unattended, repeated runs (e.g. while the user is away) to
gather more data points on whether the STALE_FRAME_REPUBLISH_AFTER fix in
proxy/src/hooks/pump.rs actually closes the proxy_normal speed/dup gap.

Usage:
    python poc\\loop_normal_verify.py [--iterations N] [--duration SECS]

Each iteration's results land under
poc\\verify-matrix\\results\\loop_normal_<timestamp>\\<iteration>\\, and a
running summary is appended to
poc\\verify-matrix\\results\\loop_normal_summary.txt after every iteration
(so partial progress is visible even if the loop is stopped early).
"""

import argparse
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

from dll_layout import BrokenDllLayoutError, DllPaths, apply_dll_mode, restore_dll_layout, stash_dll_layout

PORT = 63723
CASES = [
    ("proxy_normal", True),
    ("noproxy_normal", False),
]


class Paths:
    def __init__(self, script_dir: Path):
        self.script_dir = script_dir
        self.root = script_dir.parent.resolve()
        self.ffmpeg_dir = self.root / "ffmpeg-master-latest-win64-lgpl-shared"
        self.bin_dir = self.ffmpeg_dir / "bin"
        self.ffmpeg_exe = self.bin_dir / "ffmpeg.exe"
        self.proxy_dll_src = self.root / "output" / "ddagrab_proxy.dll"
        self.dll = DllPaths(self.root)
        self.server_exe = self.root / "target" / "release" / "dummy-tcp-server.exe"


def check_prerequisites(paths: Paths) -> None:
    missing = []
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if not paths.server_exe.exists():
        missing.append(
            f"dummy-tcp-server.exe not found: {paths.server_exe}\n"
            "        Build it first with: cargo build --release -p dummy-tcp-server"
        )
    if not paths.proxy_dll_src.exists():
        missing.append(f"Proxy DLL not found: {paths.proxy_dll_src}")

    if missing:
        for m in missing:
            print(f"[ERROR] {m}")
        sys.exit(1)


def run_case(paths: Paths, genuine_dll: Path, iter_dir: Path, label: str, use_proxy: bool, duration_secs: int) -> None:
    case_dir = iter_dir / label
    case_dir.mkdir(parents=True, exist_ok=True)

    print("=" * 60)
    print(f"[CASE] {label}  (proxy={int(use_proxy)})")
    print("=" * 60)

    apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)

    proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
    proxy_log_path.unlink(missing_ok=True)

    server_log = case_dir / "dummy_tcp_server.csv"
    print(f"[INFO] Starting dummy-tcp-server (port {PORT})")
    server_proc = subprocess.Popen(
        [
            str(paths.server_exe),
            "--listen", f"127.0.0.1:{PORT}",
            "--log", str(server_log),
            "--connections", "1",
        ],
        creationflags=subprocess.CREATE_NO_WINDOW,
    )

    time.sleep(2)  # give the server a moment to start listening

    ffmpeg_log = case_dir / "ffmpeg.log"
    ffreport_log = case_dir / "ffmpeg_report.log"
    ffmpeg_args = [
        "-hide_banner", "-y",
        "-f", "lavfi",
        "-i", "ddagrab=output_idx=0:framerate=60:video_size=1920x1080",
        "-t", str(duration_secs),
        "-c:v", "hevc_nvenc",
        "-b:v", "8000k",
        "-maxrate", "35000k",
        "-bufsize", "35000k",
        "-f", "hevc",
        f"tcp://127.0.0.1:{PORT}",
    ]

    print("[INFO] Running ffmpeg with normal privileges")
    with ffmpeg_log.open("w", encoding="utf-8", errors="replace") as log_f:
        subprocess.run(
            [str(paths.ffmpeg_exe), *ffmpeg_args],
            cwd=str(paths.bin_dir),
            stdout=log_f,
            stderr=subprocess.STDOUT,
        )

    if ffreport_log.exists():
        shutil.copy2(ffreport_log, ffmpeg_log)
        ffreport_log.unlink(missing_ok=True)

    print(f"[INFO] ffmpeg finished. Log: {ffmpeg_log}")

    time.sleep(2)  # give the server a moment to notice the connection closed

    if server_proc.poll() is None:
        server_proc.terminate()
        try:
            server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server_proc.kill()

    if use_proxy and proxy_log_path.exists():
        shutil.copy2(proxy_log_path, case_dir / "ddagrab_proxy.log")

    print(f"[INFO] Case {label} complete")
    print()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Repeatedly compare proxy_normal vs noproxy_normal, no admin/PAExec required."
    )
    parser.add_argument("--iterations", type=int, default=10, help="How many times to repeat the comparison")
    parser.add_argument("--duration", type=int, default=30, help="ffmpeg capture duration per case, in seconds")
    args = parser.parse_args()

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_prerequisites(paths)

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    run_dir = script_dir / "verify-matrix" / "results" / f"loop_normal_{ts}"
    run_dir.mkdir(parents=True, exist_ok=True)
    summary_path = script_dir / "verify-matrix" / "results" / "loop_normal_summary.txt"
    print(f"[INFO] Results will be saved under: {run_dir}")
    print(f"[INFO] Running summary: {summary_path}")
    print()

    try:
        for i in range(1, args.iterations + 1):
            iter_dir = run_dir / f"iter_{i:03d}"
            print(f"##### Iteration {i}/{args.iterations} #####")
            for label, use_proxy in CASES:
                run_case(paths, genuine_dll, iter_dir, label, use_proxy, args.duration)

            labels = ",".join(label for label, _ in CASES)
            summarize_script = script_dir / "verify-matrix" / "summarize.py"
            subprocess.run([sys.executable, str(summarize_script), str(iter_dir), labels])

            iter_summary = iter_dir / "summary.txt"
            with summary_path.open("a", encoding="utf-8") as out_f:
                out_f.write(f"\n===== Iteration {i}/{args.iterations} ({iter_dir.name}) =====\n")
                if iter_summary.exists():
                    out_f.write(iter_summary.read_text(encoding="utf-8", errors="replace"))
                else:
                    out_f.write("(no summary.txt produced)\n")

            print(f"[INFO] Iteration {i} appended to {summary_path}")
            print()
    finally:
        restore_dll_layout(paths.dll, stash_dir)

    print(f"[INFO] All {args.iterations} iterations complete.")
    print(f"[INFO] Running summary: {summary_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
