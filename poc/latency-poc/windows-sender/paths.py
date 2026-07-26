"""paths.py

Shared path resolution for the latency-poc windows-sender scripts. Mirrors
the layout used by the sibling poc/*.py scripts (start_stream.py etc.):
ffmpeg lives under <repo_root>/ffmpeg-master-latest-win64-lgpl-shared/bin.
"""

import filecmp
import shutil
import sys
from pathlib import Path

# poc/dll_layout.py is a sibling of poc/latency-poc (this script's
# grandparent), not a package -- added to sys.path rather than duplicating
# its (hardened, incident-driven) stash/restore logic here.
_POC_DIR = Path(__file__).resolve().parent.parent.parent
if str(_POC_DIR) not in sys.path:
    sys.path.insert(0, str(_POC_DIR))

from dll_layout import DllPaths  # noqa: E402


class Paths:
    def __init__(self, script_dir: Path):
        self.script_dir = script_dir
        # script_dir = <repo_root>/poc/latency-poc/windows-sender
        self.root = script_dir.parent.parent.parent.resolve()
        self.ffmpeg_dir = self.root / "ffmpeg-master-latest-win64-lgpl-shared"
        self.bin_dir = self.ffmpeg_dir / "bin"
        self.ffmpeg_exe = self.bin_dir / "ffmpeg.exe"
        # Built via `cargo build --release -p tcp-ws-relay` from the repo
        # root -- see poc/latency-poc/tcp-ws-relay.
        self.relay_exe = self.root / "target" / "release" / "tcp-ws-relay.exe"
        # ddagrab_proxy.dll: see dll_layout.py's module doc comment for why
        # this indirection (stash/swap/restore rather than just always
        # loading the proxy) exists -- it's what start_stream.py/
        # verify_matrix.py already use to get DDA-transition recovery
        # (UAC/lock-screen/session changes that would otherwise surface as
        # "DDA ReleaseFrame failed!" and kill ffmpeg's input entirely).
        self.proxy_dll_src = self.root / "output" / "ddagrab_proxy.dll"
        # ddagrab_proxy.dll is a thin shim (see proxy/src/lib.rs) that
        # LoadLibrary()s dda_hook_core.dll from its OWN directory at
        # runtime -- the actual DDA hook/recovery logic lives there, not in
        # the proxy DLL itself. Unlike avfilter-12.dll, this one isn't part
        # of dll_layout.py's stash/swap/restore cycle (it's the same file
        # regardless of proxy on/off) so it just needs to permanently sit
        # next to ffmpeg.exe. Confirmed the hard way: running with the
        # proxy "enabled" but this file missing from bin/ produces no
        # recovery at all -- the shim loads, LoadLibrary silently has
        # nothing to hook through, and a DDA session transition still kills
        # the stream instead of being recovered from.
        self.hook_core_dll_src = self.root / "output" / "dda_hook_core.dll"
        self.hook_core_dll_dest = self.bin_dir / "dda_hook_core.dll"
        self.dll = DllPaths(self.root)
        # Optional: only required when --system is passed to send_stream.py.
        # Not bundled in this repo -- fetch from Sysinternals (PsExec.exe) or
        # https://www.poweradmin.com/paexec/ (paexec.exe, no EULA click-through
        # needed) and drop it anywhere on PATH, or point --psexec-path at it.
        self.psexec_exe = shutil.which("paexec") or shutil.which("PsExec64") or shutil.which("PsExec")


def check_ffmpeg(paths: "Paths") -> None:
    if not paths.ffmpeg_exe.exists():
        print(f"[ERROR] ffmpeg.exe not found: {paths.ffmpeg_exe}")
        sys.exit(1)


def check_psexec(paths: "Paths") -> None:
    if paths.psexec_exe is None:
        print("[ERROR] PsExec/PAExec not found on PATH (needed for --system).")
        print("        Get PsExec.exe from https://learn.microsoft.com/sysinternals/downloads/psexec")
        print("        or paexec.exe from https://www.poweradmin.com/paexec/ , and put it on PATH")
        print("        (or pass --psexec-path <path to exe>).")
        sys.exit(1)


def check_relay(paths: "Paths") -> None:
    if not paths.relay_exe.exists():
        print(f"[ERROR] tcp-ws-relay.exe not found: {paths.relay_exe}")
        print("        Build it first with: cargo build --release -p tcp-ws-relay")
        sys.exit(1)


def check_proxy_dll(paths: "Paths") -> None:
    if not paths.proxy_dll_src.exists():
        print(f"[ERROR] Proxy DLL not found: {paths.proxy_dll_src}")
        print("        Build it with: cargo build --release -p ddagrab_proxy -p dda_hook_core")
        print("        (see output/README.md for the required DDAGRAB_REAL_AVFILTER_DLL / DDAGRAB_LIB_EXE env vars)")
        sys.exit(1)
    if not paths.hook_core_dll_src.exists():
        print(f"[ERROR] dda_hook_core.dll not found: {paths.hook_core_dll_src}")
        print("        Build it with: cargo build --release -p ddagrab_proxy -p dda_hook_core")
        sys.exit(1)

    # dda_hook_core.dll must permanently sit in ffmpeg's bin/ next to
    # avfilter-12.dll (whichever DLL currently has that name) -- it's not
    # part of dll_layout.py's stash/swap/restore cycle since the proxy
    # loads it by a fixed relative name regardless of which mode is active.
    # Copied (not moved) so poc/output/ keeps its own copy as the build
    # artifact.
    if not paths.hook_core_dll_dest.exists() or not filecmp.cmp(
        paths.hook_core_dll_src, paths.hook_core_dll_dest, shallow=False
    ):
        print(f"[INFO] Deploying dda_hook_core.dll to {paths.hook_core_dll_dest}")
        shutil.copy2(paths.hook_core_dll_src, paths.hook_core_dll_dest)
