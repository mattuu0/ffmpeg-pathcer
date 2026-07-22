"""verify_matrix.py

Runs ffmpeg across 4 cases (ddagrab_proxy on/off x normal/SYSTEM privilege)
and compares dummy-tcp-server send throughput against ffmpeg's own
dup/drop/speed stats.

Prerequisites:
  - Run this script itself as Administrator.
  - SYSTEM-privilege execution uses
    ffmpeg-master-latest-win64-lgpl-shared\\bin\\paexec.exe (PAExec).
  - Build dummy-tcp-server beforehand with:
      cargo build --release -p dummy-tcp-server

Output:
  Logs for each case are saved under poc\\verify-matrix\\results\\<timestamp>\\.
"""

import ctypes
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

from dll_layout import BrokenDllLayoutError, DllPaths, apply_dll_mode, restore_dll_layout, stash_dll_layout

PORT = 63723
DURATION_SECS = 30
CASES = [
    ("proxy_normal", True, False),
    ("proxy_system", True, True),
    ("noproxy_normal", False, False),
    ("noproxy_system", False, True),
]


def is_admin() -> bool:
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def run(cmd, **kwargs):
    return subprocess.run(cmd, **kwargs)


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

        self.server_exe = self.root / "target" / "release" / "dummy-tcp-server.exe"


def check_prerequisites(paths: Paths) -> None:
    missing = []
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if not paths.paexec_exe.exists():
        missing.append(f"paexec.exe not found: {paths.paexec_exe}")
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


def run_case(paths: Paths, genuine_dll: Path, results_dir: Path, label: str, use_proxy: bool, use_system: bool) -> None:
    case_dir = results_dir / label
    case_dir.mkdir(parents=True, exist_ok=True)

    print("=" * 60)
    print(f"[CASE] {label}  (proxy={int(use_proxy)} system={int(use_system)})")
    print("=" * 60)

    apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)

    # clear any proxy log left over from a previous run
    proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
    proxy_log_path.unlink(missing_ok=True)

    # --- start the dummy TCP server --------------------------------------
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

    # --- run ffmpeg --------------------------------------------------------
    # PAExec -i (attach to the interactive desktop) doesn't reliably return
    # stdout to the caller when redirected, so instead of piping stdout we
    # let ffmpeg write its own progress log to a file via the FFREPORT env var.
    ffmpeg_log = case_dir / "ffmpeg.log"
    ffreport_log = case_dir / "ffmpeg_report.log"
    ffmpeg_args = [
        "-hide_banner", "-y",
        "-f", "lavfi",
        "-i", "ddagrab=output_idx=0:framerate=60:video_size=1920x1080",
        "-t", str(DURATION_SECS),
        "-c:v", "hevc_nvenc",
        "-b:v", "8000k",
        "-maxrate", "35000k",
        "-bufsize", "35000k",
        "-f", "hevc",
        f"tcp://127.0.0.1:{PORT}",
    ]

    if use_system:
        print("[INFO] Running ffmpeg with SYSTEM privileges (via PAExec)")
        # FFREPORT's value is colon-separated (file=<path>:level=<n>), which
        # collides with the drive-letter colon in an absolute Windows path
        # (C:\...). Work around it by running ffmpeg with case_dir as the
        # working directory and pointing FFREPORT at a bare filename.
        wrapper_cmd = case_dir / "run_ffmpeg.cmd"
        wrapper_cmd.write_text(
            "@echo off\r\n"
            f'cd /d "{case_dir}"\r\n'
            f'set "FFREPORT=file={ffreport_log.name}:level=48"\r\n'
            f'"{paths.ffmpeg_exe}" {" ".join(ffmpeg_args)}\r\n',
            encoding="ascii",
        )
        run(
            [str(paths.paexec_exe), "-s", "-i", "-w", str(paths.bin_dir), str(wrapper_cmd)],
        )
        wrapper_cmd.unlink(missing_ok=True)

        # PAExec -i doesn't reliably wait for the REMOTE (SYSTEM) ffmpeg
        # process to actually exit before its own process returns -- without
        # this, a SYSTEM-privilege ffmpeg from this case can keep running
        # (and keep holding/reconnecting to this port) into the NEXT case,
        # producing exactly the kind of cascading TCP connection resets and
        # ever-worsening "speed" seen across later cases in a run (confirmed:
        # every case after the first SYSTEM one degraded further). Force-kill
        # any lingering ffmpeg.exe unconditionally after every SYSTEM case.
        run(
            ["taskkill", "/f", "/im", "ffmpeg.exe"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    else:
        print("[INFO] Running ffmpeg with normal privileges")
        with ffmpeg_log.open("w", encoding="utf-8", errors="replace") as log_f:
            run(
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

    # clean up the server if it's still around
    # (normally exits on its own via --connections 1)
    if server_proc.poll() is None:
        server_proc.terminate()
        try:
            server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server_proc.kill()

    # save the proxy log too, when the proxy was used
    if use_proxy and proxy_log_path.exists():
        shutil.copy2(proxy_log_path, case_dir / "ddagrab_proxy.log")

    print(f"[INFO] Case {label} complete")
    print()


def main() -> int:
    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)

    if not is_admin():
        print("[ERROR] This script must be run as Administrator.")
        return 1

    check_prerequisites(paths)

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    results_dir = script_dir / "verify-matrix" / "results" / ts
    results_dir.mkdir(parents=True, exist_ok=True)
    print(f"[INFO] Results will be saved to: {results_dir}")
    print()

    try:
        for label, use_proxy, use_system in CASES:
            run_case(paths, genuine_dll, results_dir, label, use_proxy, use_system)
    finally:
        restore_dll_layout(paths.dll, stash_dir)

    print("[INFO] Writing results summary")
    summarize_script = script_dir / "verify-matrix" / "summarize.py"
    labels = ",".join(label for label, _, _ in CASES)
    subprocess.run([sys.executable, str(summarize_script), str(results_dir), labels])

    print()
    print(f"[INFO] All cases complete. Results: {results_dir}")
    print(f"[INFO] Summary: {results_dir / 'summary.txt'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
