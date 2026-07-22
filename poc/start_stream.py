"""start_stream.py

Starts an actual stream of the desktop (via ddagrab), pushed over RTSP to a
separate RTSP server (e.g. MediaMTX: https://github.com/bluenviron/mediamtx)
that Android devices can then pull from directly with VLC.

Note: ffmpeg's `rtsp` muxer (output side) has no server/listen mode of its
own -- the `rtsp_flags listen` option only exists on the INPUT (demuxer)
side (confirmed by reading ffmpeg/libavformat/rtsp.c: the `listen` flag
constant is registered with `DEC` only, and rtspenc.c, the RTSP muxer, never
references it). So ffmpeg here only PUSHES to an RTSP server that's already
running (started separately, e.g. `mediamtx.exe` with its default config,
which listens on rtsp://0.0.0.0:8554 out of the box) -- it does not listen
for connections itself.

Lets you pick, at startup, the same two axes verify_matrix.py compares:
  --proxy / --no-proxy      whether ddagrab_proxy.dll is active
  --system / --normal       whether ffmpeg runs as SYSTEM (via PAExec) or
                             with this script's own (elevated) privileges

Usage (run as Administrator, with the RTSP server already running):
    python poc\\start_stream.py --proxy --normal
    python poc\\start_stream.py --proxy --system
    python poc\\start_stream.py --no-proxy --normal
    python poc\\start_stream.py --no-proxy --system

Optionally point at a non-default RTSP server address with --server-host /
--server-port (defaults to 127.0.0.1:8554, i.e. a server running on this
same PC).

Then on the Android device (same LAN), open in VLC:
    rtsp://<the RTSP server's LAN IP>:8554/live

Runs in the foreground; press Ctrl+C to stop the stream (this also cleans
up the SYSTEM-privilege ffmpeg process when --system was used, since PAExec
itself doesn't reliably forward Ctrl+C to it).
"""

import argparse
import ctypes
import socket
import subprocess
import sys
from pathlib import Path

from dll_layout import BrokenDllLayoutError, DllPaths, apply_dll_mode, restore_dll_layout, stash_dll_layout

STREAM_PATH = "live"


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


def check_prerequisites(paths: Paths, use_system: bool) -> None:
    missing = []
    if not paths.ffmpeg_exe.exists():
        missing.append(f"ffmpeg.exe not found: {paths.ffmpeg_exe}")
    if use_system and not paths.paexec_exe.exists():
        missing.append(f"paexec.exe not found: {paths.paexec_exe}")
    if not paths.proxy_dll_src.exists():
        missing.append(f"Proxy DLL not found: {paths.proxy_dll_src}")

    if missing:
        for m in missing:
            print(f"[ERROR] {m}")
        sys.exit(1)


def build_ffmpeg_args(server_host: str, server_port: int) -> list[str]:
    return [
        "-hide_banner",
        "-f", "lavfi",
        "-i", "ddagrab=output_idx=0:framerate=60:video_size=1920x1080",
        "-c:v", "hevc_nvenc",
        # Low-latency tuning: the default preset/bufsize favor quality and
        # smoothness over latency, which is fine for a stored/broadcast
        # stream but adds seconds of glass-to-glass delay for a live view.
        # -tune ll + -rc-lookahead 0 + -bf 0 disable NVENC's lookahead and
        # B-frames (both trade latency for compression efficiency); -g 60
        # forces a keyframe every ~1s (at 60fps) so VLC doesn't need to wait
        # long for one to start decoding; -bufsize kept close to -b:v so the
        # VBV buffer itself doesn't add multi-second slack.
        "-preset", "p1",
        "-tune", "ll",
        "-rc-lookahead", "0",
        "-bf", "0",
        "-g", "60",
        "-b:v", "8000k",
        "-maxrate", "10000k",
        "-bufsize", "8000k",
        "-f", "rtsp",
        "-rtsp_transport", "tcp",
        f"rtsp://{server_host}:{server_port}/{STREAM_PATH}",
    ]


def local_ip_hint() -> str:
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect(("8.8.8.8", 80))
            return s.getsockname()[0]
    except Exception:
        return "<this PC's LAN IP>"


def check_rtsp_server_reachable(host: str, port: int) -> bool:
    try:
        with socket.create_connection((host, port), timeout=3):
            return True
    except OSError:
        return False


def run_normal(paths: Paths, server_host: str, server_port: int) -> subprocess.Popen:
    ffmpeg_args = build_ffmpeg_args(server_host, server_port)
    print("[INFO] Starting ffmpeg with normal privileges")
    # Inherit this console's stdout/stderr directly so ffmpeg's live
    # progress (frame=... fps=...) shows up in real time, same as running
    # it by hand. There's no log file for this case -- if ffmpeg fails, the
    # error is already right here in the console.
    return subprocess.Popen(
        [str(paths.ffmpeg_exe), *ffmpeg_args],
        cwd=str(paths.bin_dir),
    )


def run_system(paths: Paths, wrapper_cmd: Path, server_host: str, server_port: int) -> subprocess.Popen:
    ffmpeg_args = build_ffmpeg_args(server_host, server_port)
    # No output redirection here: PAExec -i opens ffmpeg in its own console
    # window on the interactive desktop, and leaving stdout/stderr
    # unredirected means ffmpeg's live progress shows up directly in that
    # window, same as the --normal case shows it in this one. A previous
    # version piped everything to a log file instead (useful once, to catch
    # a DLL-load crash that produced no console output at all), but that
    # meant the SYSTEM console stayed blank during normal operation.
    wrapper_cmd.write_text(
        "@echo off\r\n"
        f'"{paths.ffmpeg_exe}" {" ".join(ffmpeg_args)}\r\n',
        encoding="ascii",
    )
    print("[INFO] Starting ffmpeg with SYSTEM privileges (via PAExec) -- watch the new console window for progress")
    return subprocess.Popen(
        [str(paths.paexec_exe), "-s", "-i", "-w", str(paths.bin_dir), str(wrapper_cmd)],
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Push a desktop stream (via ddagrab) to an RTSP server.")
    proxy_group = parser.add_mutually_exclusive_group(required=True)
    proxy_group.add_argument("--proxy", action="store_true", help="Use ddagrab_proxy.dll")
    proxy_group.add_argument("--no-proxy", action="store_true", help="Use the genuine avfilter-12.dll")

    priv_group = parser.add_mutually_exclusive_group(required=True)
    priv_group.add_argument("--system", action="store_true", help="Run ffmpeg as SYSTEM (via PAExec)")
    priv_group.add_argument("--normal", action="store_true", help="Run ffmpeg with this script's own privileges")

    parser.add_argument(
        "--server-host", default="127.0.0.1",
        help="RTSP server host to push to (default: 127.0.0.1, i.e. a server running on this PC)",
    )
    parser.add_argument(
        "--server-port", type=int, default=8554,
        help="RTSP server port to push to (default: 8554, MediaMTX's default)",
    )

    args = parser.parse_args()
    use_proxy = args.proxy
    use_system = args.system
    server_host = args.server_host
    server_port = args.server_port

    if not is_admin():
        print("[ERROR] This script must be run as Administrator.")
        return 1

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_prerequisites(paths, use_system)

    if not check_rtsp_server_reachable(server_host, server_port):
        print(f"[ERROR] Could not reach an RTSP server at {server_host}:{server_port}.")
        print("        Start one first (e.g. MediaMTX: run mediamtx.exe, which listens on")
        print("        rtsp://0.0.0.0:8554 by default), or pass --server-host/--server-port")
        print("        to point at wherever it's running.")
        return 1

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    ffmpeg_proc: subprocess.Popen | None = None
    wrapper_cmd = script_dir / "run_ffmpeg_stream.cmd"

    # SIGINT's default Python behavior (raise KeyboardInterrupt on the main
    # thread) is enough here -- Popen.wait() below is interruptible, and all
    # cleanup happens in the finally block regardless of how main() exits.
    # No custom handler needed; the default just needs to still be installed
    # (it always is, unless something upstream disabled it).

    try:
        apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)

        proxy_log_path = paths.bin_dir / "ddagrab_proxy.log"
        proxy_log_path.unlink(missing_ok=True)

        # The RTSP server is what Android actually connects to, so the URL
        # to share is <server's LAN IP>, not necessarily this PC's -- but
        # when the server runs on this same PC (the common case), this PC's
        # own LAN IP is exactly that address.
        ip_hint = server_host if server_host != "127.0.0.1" else local_ip_hint()
        print()
        print("=" * 60)
        print(f"[INFO] Mode: proxy={'on' if use_proxy else 'off'} privileges={'SYSTEM' if use_system else 'normal'}")
        print(f"[INFO] Pushing to RTSP server at {server_host}:{server_port}")
        print(f"[INFO] On the Android device (same LAN), open in VLC:")
        print(f"       rtsp://{ip_hint}:{server_port}/{STREAM_PATH}")
        print("[INFO] Press Ctrl+C here to stop streaming.")
        print("=" * 60)
        print()

        if use_system:
            ffmpeg_proc = run_system(paths, wrapper_cmd, server_host, server_port)
        else:
            ffmpeg_proc = run_normal(paths, server_host, server_port)

        return_code = ffmpeg_proc.wait()
        print(f"[INFO] ffmpeg exited with code {return_code}.")
    except KeyboardInterrupt:
        print("\n[INFO] Stopping stream...")
        if ffmpeg_proc is not None and ffmpeg_proc.poll() is None:
            ffmpeg_proc.terminate()
            try:
                ffmpeg_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                ffmpeg_proc.kill()
        if use_system:
            # PAExec doesn't reliably forward Ctrl+C to the remote (SYSTEM)
            # ffmpeg process, so make sure it's actually gone.
            subprocess.run(
                ["taskkill", "/f", "/im", "ffmpeg.exe"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
    finally:
        wrapper_cmd.unlink(missing_ok=True)
        restore_dll_layout(paths.dll, stash_dir)

    return 0


if __name__ == "__main__":
    sys.exit(main())
