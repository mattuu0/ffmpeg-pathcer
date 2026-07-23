"""paths.py

Shared path resolution for the latency-poc windows-sender scripts. Mirrors
the layout used by the sibling poc/*.py scripts (start_stream.py etc.):
ffmpeg lives under <repo_root>/ffmpeg-master-latest-win64-lgpl-shared/bin.
"""

import sys
from pathlib import Path


class Paths:
    def __init__(self, script_dir: Path):
        self.script_dir = script_dir
        # script_dir = <repo_root>/poc/latency-poc/windows-sender
        self.root = script_dir.parent.parent.parent.resolve()
        self.ffmpeg_dir = self.root / "ffmpeg-master-latest-win64-lgpl-shared"
        self.bin_dir = self.ffmpeg_dir / "bin"
        self.ffmpeg_exe = self.bin_dir / "ffmpeg.exe"


def check_ffmpeg(paths: "Paths") -> None:
    if not paths.ffmpeg_exe.exists():
        print(f"[ERROR] ffmpeg.exe not found: {paths.ffmpeg_exe}")
        sys.exit(1)
