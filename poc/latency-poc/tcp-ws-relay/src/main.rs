//! tcp-ws-relay
//!
//! Sits between the Windows sender (ffmpeg's `-f hevc`/`-f h264` raw
//! Annex-B output over plain TCP, see poc/latency-poc/windows-sender) and
//! the Android receiver, converting the TCP byte stream into WebSocket
//! binary frames. Runs on localhost (or any reachable host) as a separate
//! process from both ends.
//!
//! Why this exists: to let the Android side receive over WebSocket instead
//! of a raw TCP socket for this experiment, without changing anything
//! about how the Windows sender streams (it keeps connecting to a plain
//! TCP port exactly as it does today; this relay stands in as that TCP
//! server instead of the Android app doing so directly).
//!
//! Topology: Android is the WebSocket *server* here (see
//! android-viewer/tv's WebSocketServer.kt), listening on a fixed port and
//! advertising itself over mDNS/NSD -- same role Android originally played
//! for direct-TCP (listen + advertise), just one hop further down the
//! chain now. This relay is the WebSocket *client*: it discovers Android
//! via mDNS (the exact same service type + instance name
//! `NsdAdvertiser.kt` used, see mdns_discovery.py's Python twin for the
//! equivalent lookup on the Windows side) and connects out to it, then
//! forwards ffmpeg's TCP bytes over that connection as binary frames.
//!
//! Framing: each individual TCP `read()` result is forwarded verbatim as
//! one WebSocket binary frame -- no attempt is made to align frames to NAL
//! unit boundaries. This is safe because the Android side's Annex-B
//! splitter treats the incoming data as one continuous byte stream and
//! finds NAL boundaries itself via start codes, the same way it already
//! does for a raw TCP connection; it doesn't care where the chunk
//! boundaries fall.
//!
//! Only one TCP sender is expected at a time (matching the Windows script,
//! which is a single ffmpeg process), and only one Android device is
//! discovered/connected to per run (this project expects exactly one
//! receiver on the LAN, same assumption the original direct-TCP path made).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

/// Must match NsdAdvertiser.kt's SERVICE_TYPE exactly -- mdns-sd requires
/// the trailing "local." domain explicitly, same as zeroconf did on the
/// Python discovery side (see mdns_discovery.py's SERVICE_TYPE).
const MDNS_SERVICE_TYPE: &str = "_latencypoc._udp.local.";
/// Must match NsdAdvertiser.kt's INSTANCE_NAME -- Android only ever
/// registers exactly this name (no prefix matching needed on this side
/// since mdns-sd's browse already scopes to MDNS_SERVICE_TYPE).
const MDNS_INSTANCE_NAME: &str = "latencypoc-viewer";

#[derive(Parser, Debug)]
#[command(about = "Relays a raw TCP byte stream (ffmpeg's Annex-B output) to an Android device over WebSocket")]
struct Args {
    /// TCP port to listen on for the Windows sender (ffmpeg connects here).
    #[arg(long, default_value_t = 5000)]
    tcp_port: u16,

    /// Bind address for the TCP listener -- 0.0.0.0 to accept from other
    /// hosts on the LAN, 127.0.0.1 to restrict to localhost only.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Android device's WebSocket URL (e.g. ws://192.168.1.50:5001). If
    /// omitted, found automatically via mDNS -- Android advertises itself
    /// under the exact same service NsdAdvertiser.kt always has.
    #[arg(long)]
    android_ws_url: Option<String>,

    /// Seconds to wait for the Android device to appear via mDNS before
    /// giving up (only relevant when --android-ws-url is omitted).
    #[arg(long, default_value_t = 10.0)]
    discovery_timeout: f64,
}

/// Broadcast channel capacity, in number of chunks -- not bytes. If the
/// single WebSocket connection to Android falls this far behind, it starts
/// silently missing chunks (see the RecvError::Lagged handling below)
/// rather than unboundedly growing memory or blocking the TCP reader;
/// correctness for this experiment prioritizes low latency over perfect
/// delivery during a slow patch.
const BROADCAST_CAPACITY: usize = 256;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (tx, _rx) = broadcast::channel::<Vec<u8>>(BROADCAST_CAPACITY);

    let tcp_addr: SocketAddr = format!("{}:{}", args.bind, args.tcp_port)
        .parse()
        .context("invalid --bind/--tcp-port combination")?;

    let android_ws_url = match args.android_ws_url {
        Some(url) => url,
        None => {
            println!(
                "[INFO] No --android-ws-url given -- searching for the Android receiver via mDNS (timeout={}s)...",
                args.discovery_timeout
            );
            discover_android(Duration::from_secs_f64(args.discovery_timeout))
                .await
                .context(
                    "Could not find the Android receiver via mDNS. Make sure the Android app is \
                     running (it advertises itself as soon as it starts listening), and that this \
                     PC and the Android device are on the same LAN/subnet with mDNS (UDP 5353) not \
                     blocked. Alternatively, pass --android-ws-url ws://<ip>:<port> explicitly.",
                )?
        }
    };
    println!("[INFO] Android WebSocket target: {android_ws_url}");
    println!("[INFO] TCP listener (sender connects here): {tcp_addr}");

    let tcp_task = tokio::spawn(run_tcp_listener(tcp_addr, tx.clone()));
    let ws_task = tokio::spawn(run_ws_client(android_ws_url, tx.subscribe()));

    tokio::select! {
        res = tcp_task => res.context("tcp listener task panicked")??,
        res = ws_task => res.context("ws client task panicked")??,
    }

    Ok(())
}

/// Finds the Android receiver's WebSocket address via mDNS -- the same
/// service type + instance name NsdAdvertiser.kt registers under, mirroring
/// what mdns_discovery.py's discover_receiver() does on the Python side.
async fn discover_android(timeout: Duration) -> Result<String> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;
    let receiver = daemon
        .browse(MDNS_SERVICE_TYPE)
        .context("failed to start mDNS browse")?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let _ = daemon.stop_browse(MDNS_SERVICE_TYPE);
            anyhow::bail!("timed out waiting for mDNS service {MDNS_SERVICE_TYPE}");
        }

        let event = match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(event)) => event,
            Ok(Err(_)) => anyhow::bail!("mDNS browse channel closed unexpectedly"),
            Err(_) => continue, // outer loop re-checks the deadline
        };

        if let ServiceEvent::ServiceResolved(info) = event {
            if info.get_fullname().starts_with(MDNS_INSTANCE_NAME) {
                let addr = info
                    .get_addresses()
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .context("resolved service has no IPv4 address")?;
                let port = info.get_port();
                let _ = daemon.stop_browse(MDNS_SERVICE_TYPE);
                return Ok(format!("ws://{addr}:{port}"));
            }
        }
    }
}

/// Accepts TCP connections in a loop (like the Android TcpReceiver this
/// replaces) so the Windows sender can be restarted any number of times
/// without needing to restart this relay too.
async fn run_tcp_listener(addr: SocketAddr, tx: broadcast::Sender<Vec<u8>>) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind TCP {addr}"))?;

    loop {
        let (socket, peer) = listener.accept().await.context("TCP accept failed")?;
        println!("[INFO] Sender connected from {peer}");
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(socket, tx).await {
                eprintln!("[WARN] TCP connection from {peer} ended with error: {e:#}");
            } else {
                println!("[INFO] Sender {peer} disconnected");
            }
        });
    }
}

async fn handle_tcp_connection(mut socket: TcpStream, tx: broadcast::Sender<Vec<u8>>) -> Result<()> {
    // 64KiB matches the read buffer size used on the Android side --
    // comfortably larger than any single NAL this stream produces, so
    // reads rarely span more than one chunk's worth of frame data.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = socket.read(&mut buf).await.context("TCP read failed")?;
        if n == 0 {
            return Ok(()); // sender closed the connection
        }
        // Ignoring the SendError here is intentional -- it only means the
        // WebSocket client task isn't (yet, or anymore) connected to
        // Android to receive this chunk, which isn't a failure of the TCP
        // side at all.
        let _ = tx.send(buf[..n].to_vec());
    }
}

/// Connects to Android's WebSocket server and forwards every broadcast
/// chunk as a binary frame. Reconnects are NOT implemented -- if the
/// connection drops, this task ends and the process exits (see the
/// module's known-limitations note in the README); restart the relay to
/// reconnect.
async fn run_ws_client(url: String, mut rx: broadcast::Receiver<Vec<u8>>) -> Result<()> {
    let (ws_stream, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("failed to connect to Android WebSocket at {url}"))?;
    println!("[INFO] Connected to Android over WebSocket");
    let (mut write, mut read) = ws_stream.split();

    // Android never sends anything meaningful back over this connection,
    // but incoming messages (including close/ping control frames) still
    // need to be drained -- otherwise TCP flow control could eventually
    // stall once Android's own read buffer fills up. Running this
    // alongside the forwarding loop below, rather than only reading on
    // shutdown, is what keeps that from happening during normal operation.
    let drain_incoming = async {
        while let Some(msg) = read.next().await {
            if msg.is_err() {
                break;
            }
        }
    };

    let forward_chunks = async {
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    if write.send(Message::Binary(chunk)).await.is_err() {
                        break; // Android disconnected
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Fell behind the TCP source by `skipped` chunks -- see
                    // BROADCAST_CAPACITY's doc comment for why dropping
                    // them (rather than blocking the sender or growing
                    // memory) is the intended tradeoff here.
                    eprintln!("[WARN] WebSocket client lagged, skipped {skipped} chunks");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    tokio::select! {
        _ = drain_incoming => {}
        _ = forward_chunks => {}
    }

    println!("[INFO] Disconnected from Android");
    Ok(())
}
