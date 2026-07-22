"""run_all.py

Single entry point: run this with admin rights and it will
  1. Build dummy-tcp-server (only if not already built)
  2. Run verify_matrix.py (4 cases: proxy on/off x normal/SYSTEM)
  3. Summarize results into summary.txt via verify-matrix/summarize.py
all the way through, with no manual steps in between.

Usage: python run_all.py   (run from an elevated command prompt / PowerShell)
"""

import ctypes
import shutil
import subprocess
import sys
from pathlib import Path


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

    cargo = shutil.which("cargo")
    if not cargo:
        print("[ERROR] cargo not found. Please install the Rust toolchain.")
        return 1

    print("[INFO] Building dummy-tcp-server (skips quickly if already built)")
    build_result = subprocess.run(
        [cargo, "build", "--release", "-p", "dummy-tcp-server"],
        cwd=str(root),
    )
    if build_result.returncode != 0:
        print("[ERROR] Failed to build dummy-tcp-server.")
        return 1

    print()
    print("[INFO] Build complete. Starting verification.")
    print()

    verify_matrix_script = script_dir / "verify_matrix.py"
    result = subprocess.run([sys.executable, str(verify_matrix_script)])
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
