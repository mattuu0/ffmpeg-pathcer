"""send_stream.py

Latency-measurement POC -- Windows sender half.

Captures a display via ddagrab (Desktop Duplication API, through ffmpeg's
`lavfi`/`ddagrab` source -- same capture path as the rest of this repo's
poc/*.py scripts), encodes it with a low-latency H.264 or HEVC (NVENC)
profile (see --codec), and pushes it as a raw Annex-B elementary stream over
a plain TCP connection.

By default (see --no-relay to change this), this script also launches
poc/latency-poc/tcp-ws-relay as a child process on localhost and points
ffmpeg at it instead of any direct Android connection. The relay is what
actually terminates ffmpeg's TCP connection and forwards the same bytes
as WebSocket binary frames to the Android app, which is what the Android
app expects to receive in this mode (see android-viewer/tv's
WebSocketServer.kt). This script itself never speaks WebSocket -- ffmpeg
keeps writing plain TCP exactly as before, just to a localhost port that
happens to be the relay's listener instead of the Android device's.

Android is the WebSocket *server* in this topology (listening + advertising
itself over mDNS/NSD, same role it originally played for direct-TCP) --
the relay is the one that discovers Android via mDNS and connects out to
it as a WebSocket *client*. This script's own mDNS discovery
(mdns_discovery.py) is therefore unused in relay mode: it's the relay
subprocess that does mDNS discovery now, for Android rather than for
itself.

Pass --no-relay to fall back to the original direct-TCP-to-Android
behavior (with mDNS auto-discovery of the Android app's own TCP listener,
or an explicit --dest/--port) -- useful for comparing the two paths, or if
android-viewer/tv's WebSocketReceiver isn't what's running on the device.

By default (see --no-proxy to change this), ffmpeg's ddagrab runs through
ddagrab_proxy.dll rather than the genuine avfilter-12.dll -- same DLL swap
poc/start_stream.py and poc/verify_matrix.py already use (see
poc/dll_layout.py). The proxy's own recovery logic is what this project
built to survive Desktop Duplication API session invalidation (UAC prompts,
lock screen, RDP session changes, etc.) instead of ddagrab just failing
outright -- confirmed necessary here after a long-running stream hit
"DDA ReleaseFrame failed!" followed by ffmpeg treating that as EOF and
exiting cleanly (exit code 0) rather than recovering, since a plain
avfilter-12.dll has none of that recovery built in.

The Android side auto-detects which codec is on the wire from the first
parameter-set NAL it sees (an HEVC VPS vs an H.264 SPS/PPS) -- no rebuild or
manual toggle needed there. It never switches back mid-connection; a fresh
connection (i.e. restarting this script, possibly with a different --codec)
re-detects from scratch.

TCP instead of RTP/UDP: on a lossy LAN link (Wi-Fi in particular), a dropped
UDP packet tore a NAL unit apart, and MediaCodec doesn't fail cleanly on a
malformed access unit -- it either produces visibly corrupted ("gabiru")
output or silently stops producing output at all. TCP's own retransmission
means the byte stream the Android side reads is always complete and in
order, so this trades a small, variable amount of latency (whatever TCP
needs to recover from loss) for eliminating that corruption/stall class
entirely -- the right tradeoff for a demo/POC on a LAN where bandwidth is
plentiful and a huge duplication API is already doing the capture work.

There is no RTP framing at all here -- just the codec's own Annex-B NAL
stream (0x000001 start codes) written directly to the socket.

This script only drives ffmpeg (and, by default, the relay) and gives you
a live, human-readable view of encode fps / dropped-frame count / bitrate
by tailing ffmpeg's own -stats output.

Usage:
    python send_stream.py --output-idx 1 --fps 60 --bitrate 15M --codec hevc
        # default: proxy DLL for DDA recovery, auto-starts tcp-ws-relay, Android connects via WebSocket
    python send_stream.py --no-proxy
        # genuine avfilter-12.dll, no DDA-transition recovery (matches plain upstream ffmpeg behavior)
    python send_stream.py --no-relay
        # original direct-TCP-to-Android mode, auto-discovering the receiver via mDNS
    python send_stream.py --no-relay --dest 192.168.1.50 --port 5000
        # direct TCP mode, skip discovery, use a fixed address
    python send_stream.py --codec h264

Run from an elevated (Administrator) prompt -- ddagrab needs that for the
Desktop Duplication API, same as every other poc/*.py capture script here.
"""

import argparse
import ctypes
import re
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Iterator

from mdns_discovery import discover_receiver
# paths.py inserts poc/ (dll_layout.py's location) onto sys.path as a side
# effect of import -- must run before the dll_layout import below.
from paths import Paths, check_ffmpeg, check_proxy_dll, check_relay
from dll_layout import BrokenDllLayoutError, apply_dll_mode, restore_dll_layout, stash_dll_layout


def is_admin() -> bool:
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


CODEC_CONFIG = {
    "hevc": {
        "encoder": "hevc_nvenc",
        "muxer": "hevc",
        "profile": "main",
        "metadata_bsf": "hevc_metadata",
    },
    "h264": {
        "encoder": "h264_nvenc",
        "muxer": "h264",
        "profile": "baseline",
        "metadata_bsf": "h264_metadata",
    },
}


def build_ffmpeg_args(output_idx: int, fps: int, width: int, height: int,
                       bitrate: str, dest: str, port: int, codec: str) -> list[str]:
    cfg = CODEC_CONFIG[codec]
    maxrate = bitrate
    return [
        "-hide_banner",
        "-stats",
        "-f", "lavfi",
        "-i", f"ddagrab=output_idx={output_idx}:framerate={fps}:video_size={width}x{height}",
        "-c:v", cfg["encoder"],
        "-preset", "p1",
        "-tune", "ull",
        "-profile:v", cfg["profile"],
        # A keyframe every ~4s (rather than every 1s) means ~4x fewer of the
        # oversized I-frames a CBR encoder has to squeeze into the same
        # per-second bit budget as every other frame -- each one was a
        # brief size/latency spike on the wire. TCP's own retransmission
        # (see the module docstring) means there's no packet-loss reason to
        # keep keyframes frequent here; -forced-idr below still guarantees
        # this exact cadence rather than leaving it to the encoder's scene
        # -change heuristics.
        "-g", str(fps * 4),
        "-bf", "0",
        "-forced-idr", "1",
        "-rc-lookahead", "0",
        "-delay", "0",
        "-zerolatency", "1",
        "-rc", "cbr",
        "-b:v", bitrate,
        "-maxrate", maxrate,
        "-bufsize", bitrate,
        # Tried disabling intra-refresh once the GOP was stretched to 4s (it
        # exists for UDP-loss resilience, which TCP no longer needs) -- but
        # over a 4s GOP, P-frames alone visibly accumulate drift/blockiness
        # under CBR's bit ceiling before the next keyframe cleans it up,
        # which read as reduced smoothness/motion quality. Keeping
        # intra-refresh on continuously re-freshens macroblocks across every
        # frame instead of concentrating that correction into one giant
        # keyframe every 4s, trading a small constant bitrate overhead for
        # more even quality across the whole GOP -- confirmed by the user as
        # the better tradeoff for this stream.
        "-intra-refresh", "1",
        "-force_key_frames", "expr:gte(t,n_forced*4)",
        # No SDP/RTSP session here (there never was one, even back on the
        # RTP/UDP version this replaced) -- dump_extra re-inserts the
        # encoder's parameter sets (VPS+SPS+PPS for HEVC, SPS+PPS for
        # H.264) as inline NAL units in front of every keyframe, so
        # MediaCodec.configure() on the Android side can pick them straight
        # out of the byte stream instead of needing them out-of-band.
        #
        # {h264,hevc}_metadata's video_full_range_flag/colour_primaries/
        # transfer_characteristics/matrix_coefficients rewrite the VUI tags
        # actually stored in the SPS. ddagrab's capture is full-range BT.709
        # (confirmed via ffmpeg's own stream info:
        # "d3d11(pc, gbr/bt709/iec61966-2-1)" -- "pc" means full range
        # 0-255, not limited/TV range 16-235), but NVENC ignores the
        # generic -color_range/-colorspace/-color_primaries/-color_trc
        # output options entirely (confirmed for hevc_nvenc: passing them
        # produced a bitstream ffprobe still read back as color_range=tv,
        # color_space=bt470bg) -- those tag the container/stream metadata,
        # not the encoder's own VUI writer. Patching the VUI directly via
        # this bsf is what actually lands in the bitstream Android reads.
        # Getting this wrong is what caused the "washed out" colors this bsf
        # fixes -- MediaCodec assumes limited range by default, so a
        # full-range source encoded without correcting the VUI decodes with
        # lifted blacks and dimmed whites.
        "-bsf:v", f"dump_extra=freq=keyframe,{cfg['metadata_bsf']}=video_full_range_flag=1:colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1",
        "-an",
        "-f", cfg["muxer"],
        # tcp_nodelay=1 disables Nagle's algorithm -- without it, small
        # writes (e.g. a single NAL flushed immediately for low latency) can
        # sit buffered for up to ~40ms waiting to coalesce with more data,
        # which defeats the point of a low-latency encoder tune.
        #
        # send_buffer_size caps the OS socket send buffer -- left at its
        # default, the OS can happily queue several frames' worth of data
        # before backpressure ever reaches ffmpeg, which just becomes extra
        # queueing delay sitting between the encoder and the wire on a fast
        # LAN link (the buffer isn't needed to smooth over throughput here).
        # 64KiB is enough for a couple of frames at these bitrates without
        # reintroducing that queueing.
        f"tcp://{dest}:{port}?tcp_nodelay=1&send_buffer_size=65536",
    ]


STATS_RE = re.compile(
    r"frame=\s*(?P<frame>\d+)\s+fps=\s*(?P<fps>[\d.]+)\s+q=\s*(?P<q>[\-\d.]+)\s+"
    r"size=\s*(?P<size>\S+)\s+time=\s*(?P<time>\S+)\s+bitrate=\s*(?P<bitrate>\S+)\s+"
    r"(?:dup=(?P<dup>\d+)\s+)?(?:drop=(?P<drop>\d+)\s+)?speed=\s*(?P<speed>\S+)"
)


def _iter_lines(stream) -> "Iterator[str]":
    """Some processes (ffmpeg's -stats in particular) rewrite their progress
    line in place using '\\r', not '\\n' (confirmed by capturing raw output
    directly) -- iterating the stream object line-by-line would then buffer
    silently until the process exits, since Python's line iteration only
    splits on '\\n'. Read raw characters instead and split on either
    terminator ourselves."""
    buf = ""
    while True:
        chunk = stream.read(1)
        if chunk == "":
            break
        if chunk in ("\r", "\n"):
            if buf:
                yield buf
                buf = ""
        else:
            buf += chunk
    if buf:
        yield buf


def monitor_stderr(proc: subprocess.Popen, start_time: float) -> None:
    """Tails ffmpeg's own -stats lines (written to stderr) and re-prints a
    condensed, aligned view -- fps / dropped frames / bitrate / encoder
    latency proxy (speed vs realtime) -- so encode-side health is visible at
    a glance instead of buried in ffmpeg's raw banner+stats interleaving."""
    assert proc.stderr is not None
    for line in _iter_lines(proc.stderr):
        match = STATS_RE.search(line)
        if not match:
            if line.strip():
                print(f"[ffmpeg] {line}")
            continue
        g = match.groupdict()
        elapsed = time.monotonic() - start_time
        drop = g["drop"] or "0"
        dup = g["dup"] or "0"
        print(
            f"[STATS] t={elapsed:7.1f}s frame={g['frame']:>6} fps={g['fps']:>5} "
            f"bitrate={g['bitrate']:>10} drop={drop:>4} dup={dup:>4} "
            f"speed={g['speed']:>6} (speed<1x means encoder is behind realtime)"
        )


def start_relay(paths: Paths, tcp_port: int, android_ws_url: str | None, discovery_timeout: float) -> subprocess.Popen:
    """Launches tcp-ws-relay as a child process, printing its own [INFO]/
    [WARN] lines with a distinguishing prefix so they're not confused with
    ffmpeg's -stats output when both are visible in the same console."""
    args = [
        str(paths.relay_exe),
        "--tcp-port", str(tcp_port),
        "--discovery-timeout", str(discovery_timeout),
    ]
    if android_ws_url is not None:
        args += ["--android-ws-url", android_ws_url]
    proc = subprocess.Popen(
        args,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )

    def _pump_relay_output() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            line = line.rstrip("\n")
            if line:
                print(f"[relay] {line}")

    threading.Thread(target=_pump_relay_output, name="relay-output-pump", daemon=True).start()
    return proc


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Capture this PC's display via ddagrab, encode H.264 or HEVC, and stream it over TCP "
                    "(by default via a local tcp-ws-relay -> WebSocket -> Android; see --no-relay).",
    )
    parser.add_argument(
        "--no-proxy", action="store_true",
        help="Use the genuine avfilter-12.dll instead of ddagrab_proxy.dll. The proxy (default) adds "
             "recovery from Desktop Duplication API session invalidation (UAC prompts, lock screen, "
             "RDP session changes) that a long-running capture can otherwise hit as \"DDA ReleaseFrame "
             "failed!\" followed by ffmpeg exiting cleanly instead of recovering.",
    )
    parser.add_argument(
        "--no-relay", action="store_true",
        help="Skip starting tcp-ws-relay and connect directly to the Android app's own TCP listener "
             "instead (the original direct-TCP mode). Combine with --dest/--port to target a fixed "
             "address, or omit them to auto-discover the Android app via mDNS.",
    )
    parser.add_argument(
        "--relay-tcp-port", type=int, default=5000,
        help="TCP port tcp-ws-relay listens on for this script to connect to (relay mode only; default: 5000)",
    )
    parser.add_argument(
        "--android-ws-url", default=None,
        help="(relay mode only) Android device's WebSocket URL, e.g. ws://192.168.1.50:5001. If "
             "omitted, the relay finds it automatically via mDNS -- start the Android app first, "
             "then run this with no --android-ws-url at all.",
    )
    parser.add_argument(
        "--relay-discovery-timeout", type=float, default=10.0,
        help="(relay mode only) Seconds for the relay to wait for the Android app to appear via "
             "mDNS before giving up (default: 10)",
    )
    parser.add_argument(
        "--dest", default=None,
        help="(--no-relay only) Android device's LAN IPv4 address. If omitted, it's found "
             "automatically via mDNS/NSD.",
    )
    parser.add_argument(
        "--port", type=int, default=None,
        help="(--no-relay only) TCP port the Android receiver is listening on. Defaults to whatever "
             "mDNS discovery finds, or 5000 if --dest is given explicitly.",
    )
    parser.add_argument(
        "--discovery-timeout", type=float, default=10.0,
        help="(--no-relay only) Seconds to wait for the Android receiver to appear via mDNS before giving up (default: 10)",
    )
    parser.add_argument(
        "--codec", choices=["hevc", "h264"], default="hevc",
        help="Video codec to encode with (default: hevc). The Android app auto-detects which "
             "one is on the wire -- no rebuild needed to switch.",
    )
    parser.add_argument("--output-idx", type=int, default=0, help="ddagrab display output_idx to capture (default: 0, the primary display)")
    parser.add_argument("--fps", type=int, default=60, help="Capture/encode framerate (default: 60)")
    parser.add_argument("--width", type=int, default=1920)
    parser.add_argument("--height", type=int, default=1080)
    parser.add_argument("--bitrate", default="8M", help="Target/CBR bitrate, e.g. 8M (default: 8M)")
    parser.add_argument(
        "--no-system-relaunch", action="store_true",
        help="Don't re-launch ffmpeg.exe as SYSTEM via PsExec/PAExec (see --psexec-path). By default, "
             "whenever this script itself is running elevated (which is required anyway -- see "
             "is_admin() above), ffmpeg.exe is automatically re-launched as the SYSTEM account, "
             "attached to this console's own desktop session, as an experiment to see whether a "
             "SYSTEM-privileged ddagrab can capture the UAC secure desktop (a normal elevated-admin "
             "token cannot: the secure desktop is off-limits to Desktop Duplication API regardless of "
             "caller privilege, as far as this project has confirmed -- SYSTEM may not change that "
             "either, but this makes it testable). This script itself keeps running as your normal "
             "elevated user throughout; only the ffmpeg child process is re-launched as SYSTEM. If "
             "PsExec/PAExec isn't found on PATH (see --psexec-path), this falls back to a normal "
             "(non-SYSTEM) ffmpeg launch with a warning rather than failing outright.",
    )
    parser.add_argument(
        "--psexec-path", default=None,
        help="Path to PsExec.exe/PsExec64.exe/paexec.exe, used to relaunch ffmpeg.exe as SYSTEM "
             "(see --no-system-relaunch). If omitted, looked up on PATH.",
    )
    args = parser.parse_args()

    if not is_admin():
        print("[ERROR] This script must be run as Administrator (ddagrab needs it).")
        return 1

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_ffmpeg(paths)

    use_proxy = not args.no_proxy
    if use_proxy:
        check_proxy_dll(paths)

    if args.psexec_path is not None:
        paths.psexec_exe = args.psexec_path
    args.system = False
    if not args.no_system_relaunch:
        # is_admin() above already guarantees this script is elevated -- so
        # by default (no opt-out flag), always try to relaunch ffmpeg.exe as
        # SYSTEM. Missing PsExec/PAExec is a warning + fallback here rather
        # than check_psexec()'s hard sys.exit(1) (used when the user asked
        # for --system explicitly in an earlier version of this flag) --
        # this path is now the default, so a machine without PsExec
        # installed shouldn't be unable to stream at all.
        if paths.psexec_exe is None:
            print("[WARN] PsExec/PAExec not found on PATH -- ffmpeg will run as your normal elevated "
                  "user instead of SYSTEM (pass --psexec-path, or --no-system-relaunch to silence this).")
        else:
            args.system = True

    try:
        stash_dir, genuine_dll = stash_dll_layout(paths.dll)
    except BrokenDllLayoutError as e:
        print(f"[ERROR] {e}")
        return 1

    # Everything from here on must go through this try/finally -- once
    # stash_dll_layout() has copied the DLL layout aside, restore_dll_layout()
    # has to run no matter which of the several early-return paths below is
    # taken (mDNS discovery failure, relay startup failure, Ctrl+C, ...) or
    # bin/avfilter-12.dll is left swapped to the proxy build/genuine copy
    # indefinitely instead of whatever it was before this ran.
    try:
        apply_dll_mode(paths.dll, genuine_dll, paths.proxy_dll_src, use_proxy)
        return _run(paths, args, use_proxy)
    finally:
        restore_dll_layout(paths.dll, stash_dir)


def _run(paths: Paths, args: argparse.Namespace, use_proxy: bool) -> int:
    """Everything that happens with the DLL layout already swapped in -- see main()'s try/finally."""
    if args.no_relay:
        dest = args.dest
        port = args.port

        if dest is None:
            print(f"[INFO] No --dest given -- searching for the Android receiver via mDNS (timeout={args.discovery_timeout:.0f}s)...")
            result = discover_receiver(timeout_sec=args.discovery_timeout)
            if result is None:
                print("[ERROR] Could not find the Android receiver via mDNS.")
                print("        Make sure the Android app is running and listening (it advertises itself")
                print("        as soon as it starts), and that this PC and the Android device are on the")
                print("        same LAN/subnet with mDNS (UDP 5353) not blocked. Alternatively, pass")
                print("        --dest <ip> --port <port> explicitly to skip discovery.")
                return 1
            dest = result.address
            if port is None:
                port = result.port
            print(f"[INFO] Found receiver '{result.name}' at {dest}:{result.port} via mDNS")
        elif port is None:
            port = 5000

        print("=" * 70)
        print(f"[INFO] Proxy DLL: {'on' if use_proxy else 'off'}")
        print(f"[INFO] --no-relay: connecting via TCP directly to {dest}:{port} and streaming {args.codec.upper()}")
    else:
        check_relay(paths)
        dest = "127.0.0.1"
        port = args.relay_tcp_port

        print("=" * 70)
        print(f"[INFO] Proxy DLL: {'on' if use_proxy else 'off'}")
        if args.android_ws_url is not None:
            print(f"[INFO] Starting tcp-ws-relay (tcp_port={args.relay_tcp_port}, android_ws_url={args.android_ws_url})...")
        else:
            print(f"[INFO] Starting tcp-ws-relay (tcp_port={args.relay_tcp_port}), which will search for the "
                  f"Android app via mDNS (timeout={args.relay_discovery_timeout:.0f}s)...")
        relay_proc = start_relay(paths, args.relay_tcp_port, args.android_ws_url, args.relay_discovery_timeout)
        try:
            # The relay's own [INFO] lines (printed by the pump thread above)
            # confirm TCP binding immediately; mDNS discovery of Android (if
            # --android-ws-url wasn't given) can take up to --relay-discovery-
            # timeout seconds longer, but ffmpeg can connect to the relay's TCP
            # listener right away regardless -- the relay just buffers/drops
            # what it receives until its WebSocket connection to Android is
            # actually up. This fixed pause only needs to cover the TCP bind,
            # not the full Android handshake.
            time.sleep(0.5)
            if relay_proc.poll() is not None:
                print(f"[ERROR] tcp-ws-relay exited immediately (code {relay_proc.returncode}) -- see [relay] output above.")
                return 1

            print(f"[INFO] ffmpeg will connect to the relay at {dest}:{port} (streaming {args.codec.upper()})")

            return _run_ffmpeg(paths, args, dest, port)
        finally:
            if relay_proc.poll() is None:
                print("[INFO] Stopping tcp-ws-relay...")
                relay_proc.terminate()
                try:
                    relay_proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    relay_proc.kill()

    return _run_ffmpeg(paths, args, dest, port)


def _current_session_id() -> int:
    """The console session this script itself is running in -- passed to
    PsExec/PAExec's -i so the SYSTEM ffmpeg process attaches to the same
    (interactive) desktop session as this script, rather than defaulting to
    session 0 (the non-interactive services session, which has no desktop
    at all to capture)."""
    return ctypes.windll.kernel32.WTSGetActiveConsoleSessionId()


def _build_launch_argv(paths: Paths, args: argparse.Namespace, ffmpeg_args: list[str]) -> list[str]:
    """Normal launch: [ffmpeg.exe, *ffmpeg_args]. With --system: wraps that
    in a PsExec/PAExec invocation that runs ffmpeg.exe as the SYSTEM account
    (-s) attached to this session's desktop (-i <session>) -- see --system's
    help text for why (testing whether SYSTEM privilege lets ddagrab
    capture the UAC secure desktop, which an elevated-admin token cannot).
    Only ffmpeg.exe is re-launched this way; this script and the relay
    subprocess keep running as the normal elevated user throughout."""
    if not args.system:
        return [str(paths.ffmpeg_exe), *ffmpeg_args]

    psexec_argv = [str(paths.psexec_exe)]
    exe_name = Path(str(paths.psexec_exe)).stem.lower()
    if exe_name.startswith("psexec"):
        # PAExec has no EULA prompt to suppress; PsExec does on first run
        # from a given user account, and would otherwise block waiting for
        # console input that never comes (this script isn't attached to a
        # console PsExec can read a keypress from once relaunched this way).
        psexec_argv.append("-accepteula")
    psexec_argv += ["-s", "-i", str(_current_session_id()), str(paths.ffmpeg_exe), *ffmpeg_args]
    return psexec_argv


def _run_ffmpeg(paths: Paths, args: argparse.Namespace, dest: str, port: int) -> int:
    print(f"[INFO] Capture: output_idx={args.output_idx} {args.width}x{args.height}@{args.fps}fps bitrate={args.bitrate}")
    if args.system:
        print(f"[INFO] Launching ffmpeg as SYSTEM via {paths.psexec_exe} (session {_current_session_id()})")
    print("[INFO] Press Ctrl+C to stop.")
    print("=" * 70)

    ffmpeg_args = build_ffmpeg_args(
        args.output_idx, args.fps, args.width, args.height, args.bitrate, dest, port, args.codec,
    )
    launch_argv = _build_launch_argv(paths, args, ffmpeg_args)

    ffmpeg_proc = subprocess.Popen(
        launch_argv,
        cwd=str(paths.bin_dir),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    start_time = time.monotonic()
    try:
        monitor_stderr(ffmpeg_proc, start_time)
        return_code = ffmpeg_proc.wait()
        print(f"[INFO] ffmpeg exited with code {return_code}.")
    except KeyboardInterrupt:
        print("\n[INFO] Stopping stream...")
        if ffmpeg_proc.poll() is None:
            ffmpeg_proc.terminate()
            try:
                ffmpeg_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                ffmpeg_proc.kill()

    return 0


if __name__ == "__main__":
    sys.exit(main())
