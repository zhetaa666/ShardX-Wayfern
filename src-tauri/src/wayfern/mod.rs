//! Wayfern fingerprint engine bridge.
//!
//! Wayfern is a Chromium 149 fork (from DonutBrowser) that exposes a
//! `Wayfern.getFingerprint` CDP method at page level, no auth. Each launch
//! with a fresh `--user-data-dir` produces a distinct 75-field flat
//! fingerprint (canvas noise seed, WebGL renderer, screen dims, etc.).
//!
//! Flow:
//!   1. `wayfern_status()` — is the engine binary present?
//!   2. `wayfern_install()` — download from Donut CDN + unzip (~1 GB).
//!   3. `wayfern_generate_fingerprint()` — spawn headless, CDP grab, convert
//!      to ShardX FingerprintConfig, import into library, return entry.

mod cdp;
mod convert;
mod downloader;

use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use tauri::Window;

use crate::fingerprints::LibraryEntry;

#[derive(Serialize, Clone, Debug)]
pub struct WayfernStatus {
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    /// Bytes on disk if installed; used by the UI to show "Wayfern engine (1.05 GB)".
    pub size_bytes: Option<u64>,
}

#[tauri::command]
pub async fn wayfern_status() -> Result<WayfernStatus, String> {
    let bin = downloader::binary_path().ok();
    let installed = bin.as_ref().is_some_and(|p| p.exists());
    let size_bytes = if installed {
        bin.as_ref()
            .and_then(|p| p.parent())
            .and_then(|d| downloader::dir_size(d).ok())
    } else {
        None
    };
    Ok(WayfernStatus {
        installed,
        binary_path: if installed { bin } else { None },
        version: downloader::installed_version().ok(),
        size_bytes,
    })
}

#[tauri::command]
pub async fn wayfern_install(window: Window, force: bool) -> Result<WayfernStatus, String> {
    downloader::install(&window, force)
        .await
        .map_err(|e| e.to_string())?;
    wayfern_status().await
}

/// Spawn Wayfern headless, grab a fresh fingerprint via CDP, convert to a
/// ShardX FingerprintConfig, and import it into the library. Returns the
/// saved library entry so the UI can select it immediately.
#[tauri::command]
pub async fn wayfern_generate_fingerprint(label: Option<String>) -> Result<LibraryEntry, String> {
    let bin = downloader::binary_path().map_err(|e| e.to_string())?;
    if !bin.exists() {
        return Err("Wayfern engine not installed — run wayfern_install first".into());
    }

    let raw = cdp::grab_fingerprint(&bin).await.map_err(|e| e.to_string())?;
    let cfg = convert::wayfern_to_shardx(&raw, label.as_deref());
    let json = serde_json::to_string(&cfg).map_err(|e| e.to_string())?;

    crate::fingerprints::import(&json, None).map_err(|e| e.to_string())
}
