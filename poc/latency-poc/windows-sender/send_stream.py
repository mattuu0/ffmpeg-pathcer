"""send_stream.py

Latency-measurement POC -- Windows sender half.

Captures a display via ddagrab (Desktop Duplication API, through ffmpeg's
`lavfi`/`ddagrab` source -- same capture path as the rest of this repo's
poc/*.py scripts), encodes it with a low-latency H.264 or HEVC (NVENC)
profile (see --codec), and pushes it as a raw Annex-B elementary stream over
a plain TCP connection to the Android receiver.

The Android side auto-detects which codec is on the wire from the first
parameter-set NAL it sees (an HEVC VPS vs an H.264 SPS/PPS) -- no rebuild or
manual toggle needed there. It never switches back mid-connection; a fresh
TCP connection (i.e. restarting this script, possibly with a different
--codec) re-detects from scratch.

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
stream (0x000001 start codes) written directly to the socket. The Android
side reads the TCP stream and splits it back into NAL units itself (see
android-viewer/tv's AnnexBReader).

The Android app listens (server role) on a fixed TCP port and this script
connects to it (client role) -- this way the Android app can be started
once and left running, accepting a fresh connection any time this script is
(re)started, rather than needing to already have a socket open before the
sender exists.

This script only drives ffmpeg and gives you a live, human-readable view of
encode fps / dropped-frame count / bitrate by tailing ffmpeg's own -stats
output.

By default the Android receiver's IPv4 address is found automatically via
mDNS/NSD (see mdns_discovery.py) -- the Android app advertises itself as
soon as it starts listening, so just start that first, then run this with
no --dest at all. Pass --dest explicitly to skip discovery (e.g. if mDNS is
blocked on this network, or to target a fixed IP without waiting).

Usage:
    python send_stream.py                                   # auto-discover the receiver via mDNS
    python send_stream.py --dest 192.168.1.50 --port 5000    # skip discovery, use a fixed address
    python send_stream.py --codec h264                       # H.264 instead of the default HEVC
    python send_stream.py --output-idx 1
    python send_stream.py --bitrate 12M --fps 60

Run from an elevated (Administrator) prompt -- ddagrab needs that for the
Desktop Duplication API, same as every other poc/*.py capture script here.
"""

import argparse
import ctypes
import re
import subprocess
import sys
import time
from pathlib import Path

from mdns_discovery import discover_receiver
from paths import Paths, check_ffmpeg


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
        "-g", str(fps),
        "-bf", "0",
        "-forced-idr", "1",
        "-rc-lookahead", "0",
        "-delay", "0",
        "-zerolatency", "1",
        "-rc", "cbr",
        "-b:v", bitrate,
        "-maxrate", maxrate,
        "-bufsize", bitrate,
        "-intra-refresh", "1",
        "-force_key_frames", "expr:gte(t,n_forced*1)",
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
        f"tcp://{dest}:{port}?tcp_nodelay=1",
    ]


STATS_RE = re.compile(
    r"frame=\s*(?P<frame>\d+)\s+fps=\s*(?P<fps>[\d.]+)\s+q=\s*(?P<q>[\-\d.]+)\s+"
    r"size=\s*(?P<size>\S+)\s+time=\s*(?P<time>\S+)\s+bitrate=\s*(?P<bitrate>\S+)\s+"
    r"(?:dup=(?P<dup>\d+)\s+)?(?:drop=(?P<drop>\d+)\s+)?speed=\s*(?P<speed>\S+)"
)


def _iter_ffmpeg_lines(stream) -> "iter[str]":
    """ffmpeg's -stats progress line is rewritten in place using '\\r', not
    '\\n' (confirmed by capturing raw output directly) -- iterating the
    stream object line-by-line would then buffer silently until the process
    exits, since Python's line iteration only splits on '\\n'. Read raw
    characters instead and split on either terminator ourselves."""
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
    for line in _iter_ffmpeg_lines(proc.stderr):
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


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Capture this PC's display via ddagrab, encode H.264 or HEVC, and stream it over TCP.",
    )
    parser.add_argument(
        "--dest", default=None,
        help="Android device's LAN IPv4 address. If omitted, it's found automatically via "
             "mDNS/NSD -- start the Android receiver first, then run this with no --dest.",
    )
    parser.add_argument(
        "--port", type=int, default=None,
        help="TCP port the Android receiver is listening on. If --dest is also omitted, this "
             "defaults to whatever port mDNS discovery finds the receiver advertising; if "
             "--dest is given explicitly without --port, defaults to 5000.",
    )
    parser.add_argument(
        "--discovery-timeout", type=float, default=10.0,
        help="Seconds to wait for the Android receiver to appear via mDNS before giving up (default: 10)",
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
    args = parser.parse_args()

    if not is_admin():
        print("[ERROR] This script must be run as Administrator (ddagrab needs it).")
        return 1

    script_dir = Path(__file__).resolve().parent
    paths = Paths(script_dir)
    check_ffmpeg(paths)

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

    ffmpeg_args = build_ffmpeg_args(
        args.output_idx, args.fps, args.width, args.height, args.bitrate, dest, port, args.codec,
    )

    print("=" * 70)
    print(f"[INFO] Connecting via TCP to {dest}:{port} and streaming {args.codec.upper()}")
    print(f"[INFO] Capture: output_idx={args.output_idx} {args.width}x{args.height}@{args.fps}fps bitrate={args.bitrate}")
    print("[INFO] Make sure the Android receiver is already running and listening before this connects.")
    print("[INFO] Press Ctrl+C to stop.")
    print("=" * 70)

    proc = subprocess.Popen(
        [str(paths.ffmpeg_exe), *ffmpeg_args],
        cwd=str(paths.bin_dir),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    start_time = time.monotonic()
    try:
        monitor_stderr(proc, start_time)
        return_code = proc.wait()
        print(f"[INFO] ffmpeg exited with code {return_code}.")
    except KeyboardInterrupt:
        print("\n[INFO] Stopping stream...")
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    return 0


if __name__ == "__main__":
    sys.exit(main())
