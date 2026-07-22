"""test_harness_system.py

Runs test-harness.exe (the UAC secure-desktop transition + ddagrab_proxy
recovery check) under SYSTEM privileges via PAExec, so the same recovery
logic verify_matrix.py already checks under normal privileges (--normal)
can also be checked under --system.

Why this matters: SYSTEM-privilege processes normally have NO interactive
window station/desktop attached at all (they're meant to run headless), so
whether OpenInputDesktop/SetThreadDesktop/DuplicateOutput1 behave the same
way there as under a normal interactive user session is a real open
question, not something --normal testing can answer.

Prerequisites:
  - Run this script itself as Administrator (PAExec -s requires it).
  - ffmpeg-master-latest-win64-lgpl-shared\\bin\\paexec.exe (PAExec) must exist.
  - Build test-harness beforehand with:
      cargo build --release -p test-harness
  - The proxy DLL must already be deployed as
    ffmpeg-master-latest-win64-lgpl-shared\\bin\\avfilter-12.dll
    (this script does not touch the DLL layout -- it assumes whatever is
    currently deployed is what you want tested).

Output:
  test-harness's own stdout (its ffmpeg passthrough lines plus the full
  ddagrab_proxy.log dump at the end) is captured via FFREPORT-style file
  redirection, since PAExec -s -i does not reliably return console output
  to the caller. Written to poc\\verify-matrix\\results\\test_harness_system_<timestamp>\\.
"""

import argparse
import ctypes
import subprocess
import sys
from datetime import datetime
from pathlib import Path


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
        self.test_harness_exe = self.root / "target" / "release" / "test-harness.exe"
        self.proxy_log = self.bin_dir / "ddagrab_proxy.log"
        self.results_root = self.root / "poc" / "verify-matrix" / "results"


def check_prerequisites(paths: Paths) -> None:
    missing = []
    if not is_admin():
        missing.append("This script itself must run as Administrator (PAExec -s requires it).")
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if not paths.paexec_exe.exists():
        missing.append(f"paexec.exe not found: {paths.paexec_exe}")
    if not paths.test_harness_exe.exists():
        missing.append(
            f"test-harness.exe not found: {paths.test_harness_exe}\n"
            "        Build it first with: cargo build --release -p test-harness"
        )

    if missing:
        for m in missing:
            print(f"[ERROR] {m}")
        sys.exit(1)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration-secs", type=int, default=20)
    parser.add_argument("--trigger-after-secs", type=int, default=3)
    parser.add_argument("--trigger-count", type=int, default=3)
    parser.add_argument("--trigger-interval-secs", type=int, default=4)
    args = parser.parse_args()

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_prerequisites(paths)

    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    results_dir = paths.results_root / f"test_harness_system_{timestamp}"
    results_dir.mkdir(parents=True, exist_ok=True)

    stdout_log = results_dir / "test_harness_stdout.log"
    proxy_log_copy = results_dir / "ddagrab_proxy.log"

    # Clear any stale proxy log so we only capture this run's output.
    paths.proxy_log.unlink(missing_ok=True)

    # PAExec -i (attach to the interactive window station) doesn't reliably
    # forward stdout back to the caller when redirected directly, so route
    # through a wrapper .cmd that redirects test-harness's own stdout/stderr
    # to a file instead (same pattern as verify_matrix.py's FFREPORT
    # workaround, just simpler since test-harness isn't ffmpeg itself).
    wrapper_cmd = results_dir / "run_test_harness.cmd"
    test_harness_args = [
        "--duration-secs", str(args.duration_secs),
        "--trigger-after-secs", str(args.trigger_after_secs),
        "--trigger-count", str(args.trigger_count),
        "--trigger-interval-secs", str(args.trigger_interval_secs),
    ]
    wrapper_cmd.write_text(
        "@echo off\r\n"
        f'cd /d "{paths.root}"\r\n'
        f'"{paths.test_harness_exe}" {" ".join(test_harness_args)} > "{stdout_log}" 2>&1\r\n',
        encoding="ascii",
    )

    print("=" * 60)
    print("[CASE] test-harness under SYSTEM privileges (via PAExec)")
    print("=" * 60)
    print(f"[INFO] duration={args.duration_secs}s trigger_after={args.trigger_after_secs}s "
          f"trigger_count={args.trigger_count} trigger_interval={args.trigger_interval_secs}s")

    subprocess.run(
        [str(paths.paexec_exe), "-s", "-i", "-w", str(paths.root), str(wrapper_cmd)],
    )
    wrapper_cmd.unlink(missing_ok=True)

    print()
    print(f"--- test-harness stdout ({stdout_log}) ---")
    if stdout_log.exists():
        print(stdout_log.read_text(encoding="utf-8", errors="replace"))
    else:
        print("(no output captured -- PAExec may have failed to launch test-harness)")

    if paths.proxy_log.exists():
        proxy_log_copy.write_bytes(paths.proxy_log.read_bytes())
        print(f"[INFO] copied ddagrab_proxy.log to {proxy_log_copy}")
    else:
        print("[WARN] ddagrab_proxy.log not found -- the proxy DLL may not be deployed, "
              "or SYSTEM's ffmpeg process never loaded it")

    print()
    print(f"[INFO] results saved under {results_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
