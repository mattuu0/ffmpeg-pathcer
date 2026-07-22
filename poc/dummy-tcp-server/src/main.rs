use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

/// Dummy TCP sink for verifying ffmpeg `tcp://` output throughput/stability.
/// Accepts one connection at a time, discards the payload, and logs received
/// bytes / instantaneous throughput at a fixed interval. Does not parse or
/// store the stream itself -- see `--log` for the CSV throughput log.
#[derive(Parser)]
struct Cli {
    /// Address to listen on (ffmpeg connects to this as a TCP client).
    #[arg(long, default_value = "127.0.0.1:63723")]
    listen: String,

    /// How often to print/log a throughput sample, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval_ms: u64,

    /// Optional CSV log file: elapsed_ms,total_bytes,interval_bytes,interval_kbps
    #[arg(long)]
    log: Option<PathBuf>,

    /// Exit after accepting and fully draining this many connections
    /// (0 = run forever, accepting new connections indefinitely).
    #[arg(long, default_value_t = 1)]
    connections: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let listener = TcpListener::bind(&cli.listen)
        .with_context(|| format!("failed to bind {}", cli.listen))?;
    println!("dummy-tcp-server listening on {}", cli.listen);

    let mut log_file = match &cli.log {
        Some(path) => {
            let f = std::fs::File::create(path)
                .with_context(|| format!("failed to create log file {}", path.display()))?;
            let mut f = f;
            writeln!(f, "elapsed_ms,total_bytes,interval_bytes,interval_kbps")?;
            Some(f)
        }
        None => None,
    };

    let mut accepted = 0u64;
    loop {
        println!("waiting for connection...");
        let (mut stream, peer) = listener.accept().context("accept failed")?;
        accepted += 1;
        println!("[{accepted}] connection from {peer}");

        let mut buf = [0u8; 64 * 1024];
        let mut total_bytes: u64 = 0;
        let mut interval_bytes: u64 = 0;
        let start = Instant::now();
        let mut last_sample = start;
        let interval = Duration::from_millis(cli.interval_ms);

        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    println!(
                        "[{accepted}] connection closed by peer, total_bytes={total_bytes}, elapsed={:?}",
                        start.elapsed()
                    );
                    let now = Instant::now();
                    let elapsed_ms = now.duration_since(start).as_millis();
                    let dt_secs = now.duration_since(last_sample).as_secs_f64();
                    let kbps = (interval_bytes as f64 / 1024.0) / dt_secs.max(0.001);
                    if let Some(f) = log_file.as_mut() {
                        writeln!(f, "{elapsed_ms},{total_bytes},{interval_bytes},{kbps:.1}")?;
                        let _ = f.flush();
                    }
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;
                    interval_bytes += n as u64;
                }
                Err(e) => {
                    println!("[{accepted}] read error: {e}");
                    break;
                }
            }

            let now = Instant::now();
            if now.duration_since(last_sample) >= interval {
                let elapsed_ms = now.duration_since(start).as_millis();
                let dt_secs = now.duration_since(last_sample).as_secs_f64();
                let kbps = (interval_bytes as f64 / 1024.0) / dt_secs.max(0.001);
                println!(
                    "[{accepted}] t={elapsed_ms}ms total={total_bytes}B interval={interval_bytes}B rate={kbps:.1}KB/s"
                );
                if let Some(f) = log_file.as_mut() {
                    writeln!(f, "{elapsed_ms},{total_bytes},{interval_bytes},{kbps:.1}")?;
                    let _ = f.flush();
                }
                interval_bytes = 0;
                last_sample = now;
            }
        }

        if cli.connections != 0 && accepted >= cli.connections {
            println!("reached --connections={}, exiting", cli.connections);
            break;
        }
    }

    Ok(())
}
