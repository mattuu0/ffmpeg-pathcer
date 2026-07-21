use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

const RELEASES_API: &str = "https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/latest";
const ASSET_NAME_SUBSTR: &str = "win64-lgpl-shared";

#[derive(Parser)]
#[command(about = "Downloads BtbN FFmpeg-Builds and installs the ddagrab recovery proxy DLL")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Downloads (or re-uses an already-downloaded) ffmpeg build into `dest`.
    Fetch {
        #[arg(long, default_value = "ffmpeg-master-latest-win64-lgpl-shared")]
        dest: PathBuf,
        /// Re-download even if `dest` already exists.
        #[arg(long)]
        force: bool,
    },
    /// Renames the real avfilter-12.dll to avfilter-12_orig.dll and copies
    /// the built proxy DLL into its place.
    Install {
        #[arg(long, default_value = "ffmpeg-master-latest-win64-lgpl-shared")]
        ffmpeg_dir: PathBuf,
        #[arg(long)]
        proxy_dll: PathBuf,
    },
}

#[derive(Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Fetch { dest, force } => fetch(&dest, force),
        Commands::Install { ffmpeg_dir, proxy_dll } => install(&ffmpeg_dir, &proxy_dll),
    }
}

fn fetch(dest: &Path, force: bool) -> Result<()> {
    if dest.exists() && !force {
        println!(
            "{} already exists; skipping download (pass --force to re-download)",
            dest.display()
        );
        return Ok(());
    }

    println!("querying {RELEASES_API} for the latest win64-lgpl-shared build...");
    let release: Release = ureq::get(RELEASES_API)
        .set("User-Agent", "ddagrab-proxy-deployer")
        .call()
        .context("failed to query GitHub releases API")?
        .into_json()
        .context("failed to parse GitHub releases API response")?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.contains(ASSET_NAME_SUBSTR) && a.name.ends_with(".zip"))
        .with_context(|| format!("no asset containing '{ASSET_NAME_SUBSTR}' found in latest release"))?;

    println!("downloading {} ...", asset.name);
    let zip_path = std::env::temp_dir().join(&asset.name);
    download_to_file(&asset.browser_download_url, &zip_path)?;

    if dest.exists() {
        println!("removing existing {}", dest.display());
        fs::remove_dir_all(dest).context("failed to remove existing ffmpeg directory")?;
    }

    println!("extracting {} -> {}", zip_path.display(), dest.display());
    extract_zip_single_root(&zip_path, dest)?;

    println!("done: {}", dest.display());
    Ok(())
}

fn download_to_file(url: &str, dest: &Path) -> Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", "ddagrab-proxy-deployer")
        .call()
        .with_context(|| format!("failed to download {url}"))?;

    let mut file = fs::File::create(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    std::io::copy(&mut response.into_reader(), &mut file)
        .context("failed to write downloaded file")?;
    Ok(())
}

/// BtbN release zips contain a single top-level directory
/// (`ffmpeg-<version>-win64-lgpl-shared/`); this extracts its contents
/// directly into `dest` rather than nesting an extra level.
fn extract_zip_single_root(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path).context("failed to open downloaded zip")?;
    let mut archive = zip::ZipArchive::new(file).context("failed to read zip archive")?;

    fs::create_dir_all(dest).context("failed to create destination directory")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_path = entry.mangled_name();

        // Strip the single top-level directory component, if present.
        let mut components = entry_path.components();
        components.next();
        let relative: PathBuf = components.collect();
        if relative.as_os_str().is_empty() {
            continue;
        }

        let out_path = dest.join(&relative);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out_path)
                .with_context(|| format!("failed to create {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut out_file)?;
        }
    }

    Ok(())
}

fn install(ffmpeg_dir: &Path, proxy_dll: &Path) -> Result<()> {
    let bin_dir = ffmpeg_dir.join("bin");
    let real_dll = bin_dir.join("avfilter-12.dll");
    let renamed_dll = bin_dir.join("avfilter-12_orig.dll");

    if !real_dll.exists() && !renamed_dll.exists() {
        bail!(
            "neither {} nor {} exist; run `fetch` first",
            real_dll.display(),
            renamed_dll.display()
        );
    }

    if real_dll.exists() && !renamed_dll.exists() {
        println!("renaming {} -> {}", real_dll.display(), renamed_dll.display());
        fs::rename(&real_dll, &renamed_dll).context("failed to rename real avfilter-12.dll")?;
    } else if renamed_dll.exists() {
        println!(
            "{} already present; assuming proxy is already (or being re-) installed",
            renamed_dll.display()
        );
    }

    if !proxy_dll.exists() {
        bail!("proxy DLL not found at {}", proxy_dll.display());
    }

    println!("copying {} -> {}", proxy_dll.display(), real_dll.display());
    fs::copy(proxy_dll, &real_dll).context("failed to copy proxy DLL into place")?;

    println!("install complete: {} now runs the ddagrab recovery proxy", real_dll.display());
    Ok(())
}
