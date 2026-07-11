//! Wayfern binary auto-downloader.
//!
//! Fetches the manifest from Donut CDN, HEAD-probes the archive size, streams
//! the download with resume support, verifies the final size against the
//! HEAD, and unzips into `$DATA/shardx-launcher/wayfern/<version>/`. Emits
//! `wayfern:progress` and `wayfern:done` events to the frontend.
//!
//! Manifest shape (as of 2026-07): `{ version, downloads: { "windows-x64":
//! "url", ... } }`. No per-platform size field — hence the HEAD probe.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Window};
use tokio::io::AsyncWriteExt;

const MANIFEST_URL: &str = "https://donutbrowser.com/wayfern.json";
/// Donut's public browser-download UA is required — the CDN 403s empty UAs.
const CLIENT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 ShardX-Launcher";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WayfernRelease {
    pub version: String,
    pub download_url: String,
    pub size: u64,
}

pub fn wayfern_root() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("platform data dir not available")?
        .join("shardx-launcher")
        .join("wayfern"))
}

fn version_marker() -> Result<PathBuf> {
    Ok(wayfern_root()?.join("installed.json"))
}

pub fn installed_version() -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(version_marker()?)?)?;
    v.get("version")
        .and_then(|s| s.as_str())
        .map(String::from)
        .context("no version in marker")
}

/// Path to the Wayfern chrome.exe. Only valid once installed.
pub fn binary_path() -> Result<PathBuf> {
    let version = installed_version().unwrap_or_else(|_| "current".into());
    let root = wayfern_root()?.join(&version);
    #[cfg(target_os = "windows")]
    return Ok(root.join("chrome.exe"));
    #[cfg(target_os = "macos")]
    return Ok(root
        .join("Wayfern.app")
        .join("Contents")
        .join("MacOS")
        .join("Wayfern"));
    #[cfg(target_os = "linux")]
    return Ok(root.join("chrome"));
}

/// Recursively sum file sizes under `dir` — used for the "engine on disk" number.
pub fn dir_size(dir: &Path) -> Result<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in fs::read_dir(&d)?.flatten() {
            let ft = ent.file_type()?;
            if ft.is_dir() {
                stack.push(ent.path());
            } else if ft.is_file() {
                total += ent.metadata()?.len();
            }
        }
    }
    Ok(total)
}

/// Fetch the manifest, pick the archive for this host, and probe its size via
/// a HEAD request (manifest at `donutbrowser.com/wayfern.json` doesn't include
/// per-platform sizes; the CDN sends `Content-Length` + `Accept-Ranges: bytes`).
async fn fetch_release() -> Result<WayfernRelease> {
    let client = reqwest::Client::builder()
        .user_agent(CLIENT_UA)
        .build()?;
    let m: serde_json::Value = client
        .get(MANIFEST_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let version = m
        .get("version")
        .and_then(|v| v.as_str())
        .context("manifest missing version")?
        .to_string();

    let downloads = m
        .get("downloads")
        .and_then(|p| p.as_object())
        .context("manifest missing downloads map")?;

    let key = host_platform_key();
    let download_url = downloads
        .get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("no Wayfern release for host `{key}`"))?
        .to_string();

    let head = client
        .head(&download_url)
        .send()
        .await
        .context("HEAD request for Wayfern archive failed")?
        .error_for_status()?;
    let size = head
        .content_length()
        .context("Wayfern CDN response missing Content-Length")?;

    Ok(WayfernRelease { version, download_url, size })
}

fn host_platform_key() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "windows-x64";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "macos-arm64";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "macos-x64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "linux-x64";
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
    )))]
    return "unsupported";
}

/// Download the Wayfern archive and extract it under `$DATA/.../wayfern/<version>/`.
/// Reuses an in-progress `.part` file when the server supports Range requests.
pub async fn install(window: &Window, force: bool) -> Result<()> {
    let release = fetch_release().await?;
    let root = wayfern_root()?;
    fs::create_dir_all(&root)?;
    let target_dir = root.join(&release.version);

    // Fast path: already installed at this version.
    if !force && target_dir.exists() && binary_path().map(|p| p.exists()).unwrap_or(false) {
        let _ = window.emit("wayfern:done", &release.version);
        return Ok(());
    }

    let zip_path = root.join(format!("wayfern-{}.zip", release.version));
    let part_path = root.join(format!("wayfern-{}.zip.part", release.version));

    // Resume if a partial download exists and headers allow it.
    let mut received: u64 = 0;
    if !force && part_path.exists() {
        received = fs::metadata(&part_path)?.len();
        if received >= release.size {
            // Fully downloaded already; skip the network round trip.
            fs::rename(&part_path, &zip_path)?;
            received = release.size;
        }
    } else if force {
        let _ = fs::remove_file(&part_path);
        let _ = fs::remove_file(&zip_path);
    }

    if !zip_path.exists() {
        download(window, &release, &part_path, received).await?;
        fs::rename(&part_path, &zip_path)?;
    }

    // Verify size against manifest (cheap integrity check).
    let actual = fs::metadata(&zip_path)?.len();
    if actual != release.size {
        let _ = fs::remove_file(&zip_path);
        anyhow::bail!(
            "Wayfern archive size mismatch: got {actual}, expected {}",
            release.size
        );
    }

    // Wipe any previous extraction at this version dir (stale files break launch).
    if target_dir.exists() {
        let _ = fs::remove_dir_all(&target_dir);
    }
    fs::create_dir_all(&target_dir)?;

    let _ = window.emit(
        "wayfern:progress",
        serde_json::json!({
            "phase": "extract",
            "version": release.version,
            "percent": 100,
            "received": release.size,
            "total": release.size,
        }),
    );

    extract_zip(&zip_path, &target_dir)?;

    // Zip contains a top-level dir (e.g. `wayfern-149.0.7827.116_windows_x64/`).
    // Flatten it so `binary_path()` finds chrome.exe at the version dir root.
    flatten_single_child(&target_dir)?;

    // Keep the zip around briefly for debugging, then drop it — 1 GB is a lot.
    let _ = fs::remove_file(&zip_path);

    // Write install marker AFTER a successful extract (matches runtime.rs pattern).
    fs::write(
        version_marker()?,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": release.version,
            "installed_at": chrono_ish_now(),
        }))?,
    )?;

    let _ = window.emit("wayfern:done", &release.version);
    Ok(())
}

fn chrono_ish_now() -> String {
    // Avoid pulling `chrono` for one timestamp — use system time epoch seconds.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

/// Stream download with Range-resume when supported. Emits progress at ~1% granularity.
async fn download(
    window: &Window,
    release: &WayfernRelease,
    part_path: &Path,
    already: u64,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(CLIENT_UA)
        .build()?;
    let mut req = client.get(&release.download_url);
    if already > 0 {
        req = req.header("Range", format!("bytes={already}-"));
    }
    let resp = req.send().await?.error_for_status()?;

    // 200 = server ignored Range (start over); 206 = partial content honored.
    let resuming = resp.status().as_u16() == 206;
    let out_opts = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(part_path)
        .await?;
    let mut out = out_opts;

    let mut received: u64 = if resuming { already } else { 0 };
    let mut last_pct: u64 = u64::MAX;
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await? {
        out.write_all(&chunk).await?;
        received += chunk.len() as u64;
        let pct = if release.size > 0 {
            received.saturating_mul(100) / release.size
        } else {
            0
        };
        if pct != last_pct {
            last_pct = pct;
            let _ = window.emit(
                "wayfern:progress",
                serde_json::json!({
                    "phase": "download",
                    "version": release.version,
                    "percent": pct,
                    "received": received,
                    "total": release.size,
                }),
            );
        }
    }
    out.flush().await?;
    Ok(())
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let f = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(f)?;
    archive.extract(dest)?;
    Ok(())
}

/// If `dir` contains exactly one child directory and no files, move its
/// contents up one level. Handles the `wayfern-<ver>_<plat>/` wrapper.
fn flatten_single_child(dir: &Path) -> Result<()> {
    let mut sub_dir: Option<PathBuf> = None;
    let mut file_count = 0;
    let mut dir_count = 0;
    for ent in fs::read_dir(dir)?.flatten() {
        if ent.file_type()?.is_dir() {
            dir_count += 1;
            sub_dir = Some(ent.path());
        } else {
            file_count += 1;
        }
    }
    if file_count != 0 || dir_count != 1 {
        return Ok(());
    }
    let Some(sub) = sub_dir else { return Ok(()) };

    for ent in fs::read_dir(&sub)?.flatten() {
        let from = ent.path();
        let to = dir.join(ent.file_name());
        fs::rename(&from, &to)?;
    }
    let _ = fs::remove_dir(&sub);
    Ok(())
}
