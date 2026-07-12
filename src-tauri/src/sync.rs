use crate::{cookies, fingerprints, process, profile, proxy, settings, store};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::Emitter;

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

/// Local record of a deleted item so the deletion propagates to other
/// devices.  Chromium-style antidetect browsers sync deletes just like
/// edits: without this a profile removed on device A silently reappears
/// from device B's push.  We keep the tombstone until it has been pushed
/// *and* the pull cursor has advanced past our own echo, then purge it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub kind: String,
    pub id: String,
    /// "@<unix_secs>" delete marker; drives last-write-wins on the server.
    pub deleted_at: String,
    /// Whether this tombstone has been sent to the server at least once.
    #[serde(default)]
    pub pushed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TombstoneStore {
    #[serde(default)]
    items: Vec<Tombstone>,
}

fn tombstones_path() -> Result<PathBuf> {
    Ok(store::config_root()?.join("tombstones.json"))
}

fn load_tombstones() -> Result<TombstoneStore> {
    let path = tombstones_path()?;
    if !path.exists() {
        return Ok(TombstoneStore::default());
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn save_tombstones(s: &TombstoneStore) -> Result<()> {
    fs::write(tombstones_path()?, serde_json::to_string_pretty(s)?)?;
    Ok(())
}

/// Record (or refresh) a tombstone for a locally deleted item.  Called by
/// the delete commands so the next sync pushes the deletion outward.
pub fn record_tombstone(kind: &str, id: &str) -> Result<()> {
    record_tombstone_at(kind, id, &now_marker(), false)
}

/// Lower-level tombstone writer.  `pushed=true` (used when we received the
/// delete from the server) keeps the original marker and suppresses a
/// pointless re-push; the local-delete path uses a fresh marker + false.
fn record_tombstone_at(kind: &str, id: &str, marker: &str, pushed: bool) -> Result<()> {
    if id.is_empty() {
        return Ok(());
    }
    let mut s = load_tombstones()?;
    if let Some(t) = s.items.iter_mut().find(|t| t.kind == kind && t.id == id) {
        // Keep the newest marker; only downgrade `pushed` for local deletes.
        if marker >= t.deleted_at.as_str() {
            t.deleted_at = marker.to_string();
        }
        if !pushed {
            t.pushed = false;
        }
    } else {
        s.items.push(Tombstone {
            kind: kind.into(),
            id: id.into(),
            deleted_at: marker.to_string(),
            pushed,
        });
    }
    save_tombstones(&s)
}

/// Drop a tombstone once the item is recreated locally (e.g. a remote
/// upsert wins LWW), so a stale delete can't wipe the fresh copy.
fn clear_tombstone(kind: &str, id: &str) -> Result<()> {
    let mut s = load_tombstones()?;
    let before = s.items.len();
    s.items.retain(|t| !(t.kind == kind && t.id == id));
    if s.items.len() != before {
        save_tombstones(&s)?;
    }
    Ok(())
}

/// True if a tombstone for (kind,id) is newer than `marker` — used so a
/// remote upsert we already deleted locally doesn't resurrect the item.
fn tombstoned_after(kind: &str, id: &str, marker: &str) -> bool {
    load_tombstones()
        .ok()
        .map(|s| {
            s.items
                .iter()
                .any(|t| t.kind == kind && t.id == id && t.deleted_at.as_str() >= marker)
        })
        .unwrap_or(false)
}

/// Mark every tombstone as pushed after a successful `/sync/push`.  Once
/// pushed, they stay until the item's own upsert is confirmed gone from
/// incoming changes; we simply purge pushed tombstones older than a grace
/// window so the file can't grow forever.
fn mark_tombstones_pushed() -> Result<()> {
    let mut s = load_tombstones()?;
    if s.items.is_empty() {
        return Ok(());
    }
    let cutoff = unix_now().saturating_sub(TOMBSTONE_TTL_SECS);
    let mut changed = false;
    for t in s.items.iter_mut() {
        if !t.pushed {
            t.pushed = true;
            changed = true;
        }
    }
    // Purge pushed tombstones older than the TTL: by then every device has
    // pulled the delete, and the server keeps its own copy for late joiners.
    let before = s.items.len();
    s.items.retain(|t| {
        let secs = t
            .deleted_at
            .strip_prefix('@')
            .and_then(|d| d.parse::<u64>().ok())
            .unwrap_or(0);
        !(t.pushed && secs < cutoff)
    });
    if s.items.len() != before {
        changed = true;
    }
    if changed {
        save_tombstones(&s)?;
    }
    Ok(())
}

/// Grace window before a pushed tombstone is purged locally (7 days).
const TOMBSTONE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncRuntimeStatus {
    pub active: bool,
    pub phase: String,
    pub profile_id: Option<String>,
    pub message: String,
    pub started_at_ms: u64,
    pub last_report: Option<SyncReport>,
    pub locked_profiles: Vec<String>,
}

fn runtime_state() -> &'static Mutex<SyncRuntimeStatus> {
    static STATE: OnceLock<Mutex<SyncRuntimeStatus>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SyncRuntimeStatus::default()))
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn runtime_status() -> SyncRuntimeStatus {
    runtime_state().lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn is_active() -> bool {
    runtime_status().active
}

pub fn is_profile_locked(profile_id: &str) -> bool {
    let s = runtime_status();
    s.active && (s.locked_profiles.is_empty() || s.locked_profiles.iter().any(|id| id == profile_id))
}

fn emit_status(status: &SyncRuntimeStatus) {
    if let Some(w) = crate::main_window() {
        let _ = w.emit("sync-progress", status);
    }
}

fn begin_sync(phase: &str, profile_id: Option<String>, message: &str) -> Result<()> {
    let mut s = runtime_state().lock().map_err(|_| anyhow!("sync state lock poisoned"))?;
    if s.active {
        return Err(anyhow!("sync already running"));
    }
    *s = SyncRuntimeStatus {
        active: true,
        phase: phase.into(),
        locked_profiles: profile_id.iter().cloned().collect(),
        profile_id,
        message: message.into(),
        started_at_ms: unix_now_ms(),
        last_report: s.last_report.clone(),
    };
    emit_status(&s);
    Ok(())
}

fn update_sync(phase: &str, message: &str) {
    if let Ok(mut s) = runtime_state().lock() {
        s.phase = phase.into();
        s.message = message.into();
        emit_status(&s);
    }
}

fn finish_sync(report: Option<SyncReport>, message: &str) {
    if let Ok(mut s) = runtime_state().lock() {
        s.active = false;
        s.phase = if report.is_some() { "done".into() } else { "error".into() };
        s.message = message.into();
        if report.is_some() {
            s.last_report = report;
        }
        s.locked_profiles.clear();
        emit_status(&s);
    }
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
    if let Err(e) = run_sync("startup", None, "Pulling latest sync on startup").await {
        eprintln!("[sync] startup sync failed: {e}");
    }
}

pub async fn sync_after_profile_close(profile_id: String) {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() {
        return;
    }
    let label = format!("Waiting for profile {profile_id} storage flush");
    if let Err(e) = begin_sync("after_close", Some(profile_id.clone()), &label) {
        eprintln!("[sync] after close {profile_id} skipped: {e}");
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    match sync_inner().await {
        Ok(r) => {
            eprintln!("[sync] after close {profile_id}: pushed {}, pulled {}, skipped {}", r.pushed, r.pulled, r.skipped);
            finish_sync(Some(r), "Sync complete");
        }
        Err(e) => {
            eprintln!("[sync] after close {profile_id} failed: {e}");
            finish_sync(None, &format!("Sync failed: {e}"));
        }
    }
}

pub async fn sync_now() -> Result<SyncReport> {
    run_sync("manual", None, "Starting sync").await
}

async fn run_sync(phase: &str, profile_id: Option<String>, message: &str) -> Result<SyncReport> {
    begin_sync(phase, profile_id, message)?;
    match sync_inner().await {
        Ok(report) => {
            finish_sync(Some(report.clone()), "Sync complete");
            Ok(report)
        }
        Err(e) => {
            finish_sync(None, &format!("Sync failed: {e}"));
            Err(e)
        }
    }
}

async fn sync_inner() -> Result<SyncReport> {
    let mut s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    update_sync("collecting", "Collecting local sync data");
    let local = collect_local(&s)?;
    let c = client();

    update_sync("pushing", &format!("Pushing {} local item(s)", local.len()));
    auth(c.post(format!("{base}/sync/push")), &s.sync_token)
        .json(&SyncBatch {
            items: local.clone(),
            cursor: s.sync_last_cursor.clone(),
        })
        .send()
        .await?
        .error_for_status()?;
    // Push succeeded: the server now owns our tombstones, so mark them
    // pushed and purge any that have aged out of the grace window.
    let _ = mark_tombstones_pushed();

    update_sync("pulling", "Pulling remote changes");
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

    update_sync("applying", &format!("Applying {} remote item(s)", remote.items.len()));
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

    // Deletions: push a tombstone for every locally removed item so the
    // delete wins last-write-wins on the server and reaches other devices.
    // Only unpushed tombstones go out — re-pushing an already-stored one
    // with an equal marker would bump the server seq and echo back forever,
    // creating a two-device sync loop.  The server's normalizeItem rejects
    // null payloads, so carry a minimal marker object instead.
    for t in load_tombstones()?.items {
        if t.pushed {
            continue;
        }
        out.push(SyncItem {
            kind: t.kind.clone(),
            id: t.id.clone(),
            updated_at: t.deleted_at.clone(),
            deleted_at: Some(t.deleted_at.clone()),
            device_id: device.clone(),
            revision: 0,
            payload: json!({ "deleted": true, "id": t.id }),
        });
    }

    Ok(out)
}

fn apply_remote(items: &[SyncItem]) -> Result<(usize, usize)> {
    let mut applied = 0;
    let mut skipped = 0;
    for item in items {
        // Remote deletion: apply it locally so a profile/proxy/fingerprint
        // removed on another device disappears here too.  cookies/storage
        // tombstones are meaningless on their own (they follow the profile),
        // so only act on the top-level kinds.
        if let Some(deleted_at) = item.deleted_at.as_ref() {
            // Persist the server's tombstone locally (marker preserved, already
            // pushed) so a later live echo of the same item can't resurrect it.
            let remember = |kind: &str| record_tombstone_at(kind, &item.id, deleted_at, true);
            match item.kind.as_str() {
                "profile" => {
                    if process::Tracker::shared().is_running(&item.id) {
                        skipped += 1;
                        continue;
                    }
                    let _ = profile::delete(&item.id);
                    remember("profile")?;
                    applied += 1;
                }
                "proxy" => {
                    let _ = proxy::delete(&item.id);
                    remember("proxy")?;
                    applied += 1;
                }
                "fingerprint" => {
                    let _ = fingerprints::delete(&item.id);
                    remember("fingerprint")?;
                    applied += 1;
                }
                _ => skipped += 1,
            }
            continue;
        }
        // A live upsert arrived.  If we hold a tombstone at least as new as
        // this item, our delete wins LWW — skip the resurrection.
        if tombstoned_after(&item.kind, &item.id, &item.updated_at) {
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
                clear_tombstone("profile", &stored.meta.id)?;
                applied += 1;
            }
            "proxy" => {
                let p: proxy::ProxyEntry = serde_json::from_value(item.payload.clone())?;
                let pid = p.id.clone();
                proxy::upsert(p)?;
                clear_tombstone("proxy", &pid)?;
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
                let fid = entry.id.clone();
                fs::write(path, serde_json::to_string_pretty(&entry)?)?;
                clear_tombstone("fingerprint", &fid)?;
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
