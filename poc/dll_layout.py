"""dll_layout.py

Shared, hardened logic for stashing/restoring/swapping the
avfilter-12.dll / avfilter-12_orig.dll layout that both verify_matrix.py and
start_stream.py use to switch ddagrab_proxy on and off.

Why this exists: an earlier version of this logic (duplicated in both
scripts) left the real ffmpeg bin/ directory in a broken state after a
Ctrl+C landed mid-restore -- avfilter-12.dll ended up holding a copy of the
(266KB) proxy DLL with no avfilter-12_orig.dll for it to forward exports to,
so ffmpeg failed to even start (STATUS_DLL_NOT_FOUND) until the ~30MB
genuine DLL was manually re-extracted from the original download archive.
This module fixes that by:
  - verifying the genuine DLL is plausibly real (size-based sanity check)
    before ever trusting a stash to restore from,
  - writing swapped-in DLLs to a temp name and atomically renaming into
    place, so a mid-copy interruption can never leave a partial file at the
    real path,
  - suppressing Ctrl+C for the duration of the restore itself, so restore
    can't be interrupted halfway the way the original bug depended on.
"""

import shutil
import signal
import tempfile
from pathlib import Path

# The genuine avfilter-12.dll bundles all of libavfilter and is tens of MB;
# ddagrab_proxy.dll only forwards exports and is a few hundred KB. Anything
# smaller than this is almost certainly a proxy build, not the genuine one --
# used to fail loudly instead of silently stashing/restoring a broken layout.
MIN_GENUINE_DLL_BYTES = 5 * 1024 * 1024


class BrokenDllLayoutError(RuntimeError):
    """Raised when the DLL layout is in a state this module refuses to
    trust (e.g. a suspiciously small "genuine" DLL) -- surfacing this as an
    explicit error is the whole point: silently proceeding is exactly what
    produced the original STATUS_DLL_NOT_FOUND incident."""


class DllPaths:
    def __init__(self, root: Path):
        self.bin_dir = root / "ffmpeg-master-latest-win64-lgpl-shared" / "bin"
        self.real_dll = self.bin_dir / "avfilter-12.dll"
        self.orig_dll = self.bin_dir / "avfilter-12_orig.dll"
        self.proxy_backup_dll = self.bin_dir / "avfilter-12_proxy_backup.dll"


def _atomic_copy(src: Path, dest: Path) -> None:
    """Copies src to dest via a temp file + rename, so a process kill
    mid-copy can never leave a truncated file sitting at `dest`."""
    tmp = dest.with_name(dest.name + ".tmp")
    shutil.copy2(src, tmp)
    tmp.replace(dest)


def _require_plausibly_genuine(dll_path: Path) -> None:
    size = dll_path.stat().st_size
    if size < MIN_GENUINE_DLL_BYTES:
        raise BrokenDllLayoutError(
            f"{dll_path} is only {size} bytes -- too small to plausibly be the genuine "
            f"avfilter-12.dll (expected at least {MIN_GENUINE_DLL_BYTES} bytes). Refusing "
            "to use it, to avoid repeating a past incident where a proxy DLL got stashed "
            "and restored as if it were genuine, leaving ffmpeg unable to start at all. "
            "If bin/avfilter-12.dll is currently broken, re-extract the genuine one from "
            "the original BtbN ffmpeg-builds download."
        )


def stash_dll_layout(paths: DllPaths) -> tuple[Path, Path]:
    """Stashes the current DLL layout and returns (stash_dir, genuine_dll).

    Raises BrokenDllLayoutError instead of stashing a proxy DLL as if it
    were genuine.
    """
    stash_dir = Path(tempfile.mkdtemp(prefix="ddagrab_stash_"))
    print(f"[INFO] Stashing current DLL layout -> {stash_dir}")

    if paths.real_dll.exists():
        _atomic_copy(paths.real_dll, stash_dir / "avfilter-12.dll")
    if paths.orig_dll.exists():
        _atomic_copy(paths.orig_dll, stash_dir / "avfilter-12_orig.dll")
    if paths.proxy_backup_dll.exists():
        _atomic_copy(paths.proxy_backup_dll, stash_dir / "avfilter-12_proxy_backup.dll")

    # Pin down the genuine (non-proxy) avfilter-12.dll. We don't know up front
    # whether the current avfilter-12.dll is the proxy or the real one. If
    # avfilter-12_orig.dll exists, that's the genuine one; otherwise the
    # current avfilter-12.dll is assumed genuine. Either way we keep a copy
    # under stash_dir, since swapping deletes real_dll/orig_dll first.
    genuine_dll = stash_dir / "avfilter-12_genuine.dll"
    source_for_genuine = (
        stash_dir / "avfilter-12_orig.dll"
        if (stash_dir / "avfilter-12_orig.dll").exists()
        else stash_dir / "avfilter-12.dll"
    )
    if not source_for_genuine.exists():
        shutil.rmtree(stash_dir, ignore_errors=True)
        raise BrokenDllLayoutError(
            f"Neither {paths.orig_dll} nor {paths.real_dll} exist -- nothing to stash. "
            "Is the ffmpeg build present?"
        )

    _atomic_copy(source_for_genuine, genuine_dll)
    _require_plausibly_genuine(genuine_dll)

    return stash_dir, genuine_dll


def restore_dll_layout(paths: DllPaths, stash_dir: Path) -> None:
    """Restores the DLL layout from a stash produced by stash_dll_layout.

    Runs with SIGINT (Ctrl+C) temporarily ignored, so a second Ctrl+C during
    restore can't interrupt it halfway -- that interruption is exactly what
    caused the original incident this module exists to prevent.
    """
    print("[INFO] Restoring DLL layout from stash")

    previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        for p in (paths.real_dll, paths.orig_dll, paths.proxy_backup_dll):
            p.unlink(missing_ok=True)

        if (stash_dir / "avfilter-12.dll").exists():
            _atomic_copy(stash_dir / "avfilter-12.dll", paths.real_dll)
        if (stash_dir / "avfilter-12_orig.dll").exists():
            _atomic_copy(stash_dir / "avfilter-12_orig.dll", paths.orig_dll)
        if (stash_dir / "avfilter-12_proxy_backup.dll").exists():
            _atomic_copy(stash_dir / "avfilter-12_proxy_backup.dll", paths.proxy_backup_dll)
    finally:
        signal.signal(signal.SIGINT, previous_handler)

    shutil.rmtree(stash_dir, ignore_errors=True)


def apply_dll_mode(paths: DllPaths, genuine_dll: Path, proxy_dll_src: Path, use_proxy: bool) -> None:
    """Swaps the real avfilter-12.dll for the proxy build, or leaves the
    genuine one in place, atomically either way.

    `genuine_dll` MUST be a copy living outside paths.real_dll/orig_dll
    (e.g. the one stash_dll_layout returns, under its own stash directory) --
    this function deletes both of those paths before copying from
    `genuine_dll`, so passing either of them in directly would delete the
    only copy of the genuine DLL before it could be read. Confirmed the hard
    way: doing exactly that manually once left bin/avfilter-12.dll missing
    entirely (ffmpeg wouldn't start at all) until re-restored from a stash
    left over by a different run.
    """
    if genuine_dll.resolve() in (paths.real_dll.resolve(), paths.orig_dll.resolve()):
        raise BrokenDllLayoutError(
            f"genuine_dll ({genuine_dll}) must not be paths.real_dll or paths.orig_dll -- "
            "it would be deleted before use. Pass the stash copy stash_dll_layout returned."
        )

    paths.real_dll.unlink(missing_ok=True)
    paths.orig_dll.unlink(missing_ok=True)

    if use_proxy:
        print("[INFO] Switching avfilter-12.dll to the proxy build")
        _atomic_copy(genuine_dll, paths.orig_dll)
        _atomic_copy(proxy_dll_src, paths.real_dll)
    else:
        print("[INFO] Using the genuine avfilter-12.dll as-is")
        _atomic_copy(genuine_dll, paths.real_dll)
