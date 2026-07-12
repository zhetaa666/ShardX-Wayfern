use crate::{cookies, fingerprints, process, profile, proxy, settings, store};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncItem {
    pub kind: String,
    pub id: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    pub device_id: String,
    #[serde(default)]
    pub revision: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncBatch {
    #[serde(default)]
    pub items: Vec<SyncItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    pub skipped: usize,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub enabled: bool,
    pub base_url: Option<String>,
    pub device_id: String,
    pub last_cursor: Option<String>,
    pub include_cookies: bool,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_marker() -> String {
    format!("@{}", unix_now())
}

fn mtime_marker(path: &Path) -> String {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("@{}", d.as_secs()))
        .unwrap_or_else(now_marker)
}

fn clean_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(anyhow!("sync server URL is empty"));
    }
    let url = url::Url::parse(trimmed).context("invalid sync server URL")?;
    match url.scheme() {
        "https" => Ok(trimmed.to_string()),
        "http" => {
            let host = url.host_str().unwrap_or("");
            if matches!(host, "localhost" | "127.0.0.1" | "::1")
                || host.starts_with("192.168.")
                || host.starts_with("10.")
                || host.starts_with("172.16.")
                || host.starts_with("172.17.")
                || host.starts_with("172.18.")
                || host.starts_with("172.19.")
                || host.starts_with("172.2")
                || host.starts_with("172.30.")
                || host.starts_with("172.31.")
            {
                Ok(trimmed.to_string())
            } else {
                Err(anyhow!("sync server must use HTTPS outside localhost/LAN"))
            }
        }
        _ => Err(anyhow!("sync server URL must be http(s)")),
    }
}

fn ensure_device_id(mut s: settings::Settings) -> Result<settings::Settings> {
    if s.sync_device_id.trim().is_empty() {
        s.sync_device_id = uuid::Uuid::new_v4().to_string();
        settings::save(&s)?;
    }
    Ok(s)
}

fn configured() -> Result<settings::Settings> {
    let s = ensure_device_id(settings::load()?)?;
    if !s.sync_enabled {
        return Err(anyhow!("sync is disabled"));
    }
    let base = s.sync_base_url.as_deref().unwrap_or("");
    clean_base_url(base)?;
    if s.sync_token.trim().is_empty() {
        return Err(anyhow!("sync token is empty"));
    }
    Ok(s)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}

fn auth(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.bearer_auth(token.trim())
}

pub fn status() -> Result<SyncStatus> {
    let s = ensure_device_id(settings::load()?)?;
    Ok(SyncStatus {
        enabled: s.sync_enabled,
        base_url: s.sync_base_url,
        device_id: s.sync_device_id,
        last_cursor: s.sync_last_cursor,
        include_cookies: s.sync_include_cookies,
    })
}

pub async fn test_connection(base_url: String, token: String) -> Result<Value> {
    let base = clean_base_url(&base_url)?;
    let res = auth(client().get(format!("{base}/health")), &token)
        .send()
        .await?
        .error_for_status()?;
    Ok(res.json::<Value>().await.unwrap_or_else(|_| json!({ "ok": true })))
}

pub async fn sync_on_startup() {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() {
        return;
    }
    if let Err(e) = sync_now().await {
        eprintln!("[sync] startup sync failed: {e}");
    }
}

pub async fn sync_after_profile_close(profile_id: String) {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    match sync_now().await {
        Ok(r) => eprintln!("[sync] after close {profile_id}: pushed {}, pulled {}, skipped {}", r.pushed, r.pulled, r.skipped),
        Err(e) => eprintln!("[sync] after close {profile_id} failed: {e}"),
    }
}

pub async fn sync_now() -> Result<SyncReport> {
    let mut s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    let local = collect_local(&s)?;
    let c = client();

    auth(c.post(format!("{base}/sync/push")), &s.sync_token)
        .json(&SyncBatch {
            items: local.clone(),
            cursor: s.sync_last_cursor.clone(),
        })
        .send()
        .await?
        .error_for_status()?;

    let mut req = c.get(format!("{base}/sync/changes"));
    if let Some(cursor) = s.sync_last_cursor.as_ref() {
        req = req.query(&[("since", cursor)]);
    }
    let remote = auth(req, &s.sync_token)
        .send()
        .await?
        .error_for_status()?
        .json::<SyncBatch>()
        .await?;

    let apply = apply_remote(&remote.items)?;
    if remote.cursor != s.sync_last_cursor {
        s.sync_last_cursor = remote.cursor.clone();
        settings::save(&s)?;
    }

    Ok(SyncReport {
        pushed: local.len(),
        pulled: apply.0,
        skipped: apply.1,
        cursor: remote.cursor,
    })
}

fn collect_local(s: &settings::Settings) -> Result<Vec<SyncItem>> {
    let mut out = Vec::new();
    let device = s.sync_device_id.clone();

    for meta in profile::list_all()? {
        let path = store::profiles_dir()?.join(format!("{}.json", meta.id));
        let stored = profile::load_raw(&meta.id)?;
        out.push(SyncItem {
            kind: "profile".into(),
            id: meta.id.clone(),
            updated_at: mtime_marker(&path),
            deleted_at: None,
            device_id: device.clone(),
            revision: 0,
            payload: serde_json::to_value(stored)?,
        });
        if s.sync_include_cookies && !process::Tracker::shared().is_running(&meta.id) {
            let ck = cookies::export(&meta.id).unwrap_or_default();
            out.push(SyncItem {
                kind: "cookies".into(),
                id: meta.id.clone(),
                updated_at: mtime_marker(&profile::user_data_dir(&meta.id)?.join("Default").join("Network").join("Cookies")),
                deleted_at: None,
                device_id: device.clone(),
                revision: 0,
                payload: json!({ "profile_id": meta.id, "cookies": ck }),
            });
            let bundle = export_storage_bundle(&meta.id).unwrap_or_default();
            if !bundle.is_empty() {
                out.push(SyncItem {
                    kind: "storage_bundle".into(),
                    id: meta.id.clone(),
                    updated_at: storage_updated_at(&meta.id)?,
                    deleted_at: None,
                    device_id: device.clone(),
                    revision: 0,
                    payload: json!({ "profile_id": meta.id, "format": "tar.gz+base64", "bytes": B64.encode(bundle) }),
                });
            }
        }
    }

    for p in proxy::list()? {
        out.push(SyncItem {
            kind: "proxy".into(),
            id: p.id.clone(),
            updated_at: mtime_marker(&store::proxies_path()?),
            deleted_at: None,
            device_id: device.clone(),
            revision: 0,
            payload: serde_json::to_value(p)?,
        });
    }

    for fp in fingerprints::list_all()? {
        let path = store::fingerprints_dir()?.join(format!("{}.json", fp.id));
        out.push(SyncItem {
            kind: "fingerprint".into(),
            id: fp.id.clone(),
            updated_at: mtime_marker(&path),
            deleted_at: None,
            device_id: device.clone(),
            revision: 0,
            payload: serde_json::to_value(fp)?,
        });
    }

    Ok(out)
}

fn apply_remote(items: &[SyncItem]) -> Result<(usize, usize)> {
    let mut applied = 0;
    let mut skipped = 0;
    for item in items {
        if item.deleted_at.is_some() {
            skipped += 1;
            continue;
        }
        match item.kind.as_str() {
            "profile" => {
                let mut stored: profile::StoredProfile = serde_json::from_value(item.payload.clone())?;
                if stored.meta.id.is_empty() {
                    stored.meta.id = item.id.clone();
                }
                if process::Tracker::shared().is_running(&stored.meta.id) {
                    skipped += 1;
                    continue;
                }
                profile::save_raw(&mut stored)?;
                applied += 1;
            }
            "proxy" => {
                let p: proxy::ProxyEntry = serde_json::from_value(item.payload.clone())?;
                proxy::upsert(p)?;
                applied += 1;
            }
            "proxies" => {
                let store_val: proxy::ProxyStore = serde_json::from_value(item.payload.clone())?;
                for p in store_val.proxies {
                    proxy::upsert(p)?;
                }
                applied += 1;
            }
            "fingerprint" => {
                let entry: fingerprints::LibraryEntry = serde_json::from_value(item.payload.clone())?;
                let path = store::fingerprints_dir()?.join(format!("{}.json", entry.id));
                fs::write(path, serde_json::to_string_pretty(&entry)?)?;
                applied += 1;
            }
            "cookies" => {
                let profile_id = item
                    .payload
                    .get("profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&item.id);
                if process::Tracker::shared().is_running(profile_id) {
                    skipped += 1;
                    continue;
                }
                let cookies = item
                    .payload
                    .get("cookies")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let cookies: Vec<cookies::Cookie> = serde_json::from_value(cookies)?;
                cookies::import(profile_id, &cookies)?;
                applied += 1;
            }
            "storage_bundle" => {
                let profile_id = item
                    .payload
                    .get("profile_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&item.id);
                if process::Tracker::shared().is_running(profile_id) {
                    skipped += 1;
                    continue;
                }
                let Some(s) = item.payload.get("bytes").and_then(|v| v.as_str()) else {
                    skipped += 1;
                    continue;
                };
                let bytes = B64.decode(s)?;
                import_storage_bundle(profile_id, &bytes)?;
                applied += 1;
            }
            _ => skipped += 1,
        }
    }
    Ok((applied, skipped))
}

fn storage_updated_at(profile_id: &str) -> Result<String> {
    let udd = profile::user_data_dir(profile_id)?;
    let mut latest = String::from("@0");
    for path in storage_paths(&udd) {
        let marker = mtime_marker(&path);
        if marker > latest {
            latest = marker;
        }
    }
    Ok(if latest == "@0" { now_marker() } else { latest })
}

fn storage_paths(root: &Path) -> Vec<PathBuf> {
    [
        "Default/History",
        "Default/History-journal",
        "Default/Local Storage",
        "Default/IndexedDB",
        "Default/Session Storage",
        "Default/Extension State",
        "Default/Local Extension Settings",
    ]
    .into_iter()
    .map(|p| root.join(p))
    .filter(|p| p.exists())
    .collect()
}

pub fn export_storage_bundle(profile_id: &str) -> Result<Vec<u8>> {
    if process::Tracker::shared().is_running(profile_id) {
        return Err(anyhow!("stop the profile before exporting storage"));
    }
    let udd = profile::user_data_dir(profile_id)?;
    let enc = GzEncoder::new(Vec::new(), Compression::fast());
    let mut tar = tar::Builder::new(enc);
    for path in storage_paths(&udd) {
        let rel = path.strip_prefix(&udd).unwrap();
        if path.is_dir() {
            tar.append_dir_all(rel, &path)?;
        } else {
            tar.append_path_with_name(&path, rel)?;
        }
    }
    let enc = tar.into_inner()?;
    Ok(enc.finish()?)
}

pub fn import_storage_bundle(profile_id: &str, bytes: &[u8]) -> Result<()> {
    if process::Tracker::shared().is_running(profile_id) {
        return Err(anyhow!("stop the profile before importing storage"));
    }
    let udd = profile::user_data_dir(profile_id)?;
    let dec = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(udd)?;
    Ok(())
}
