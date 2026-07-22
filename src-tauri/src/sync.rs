use crate::{cookies, fingerprints, process, profile, proxy, settings, store};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::Emitter;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const SYNC_PROTOCOL: u64 = 2;
const LEASE_TTL_SECS: u64 = 180;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncItem {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    pub device_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncBatch {
    #[serde(default)]
    pub items: Vec<SyncItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PushResponse {
    #[serde(default)]
    accepted: Vec<SyncItem>,
    #[serde(default)]
    conflicts: Vec<SyncConflict>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SyncConflict {
    kind: String,
    id: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    server_item: Option<SyncItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct JournalEntry {
    revision: u64,
    checksum: String,
    #[serde(default)]
    deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SyncJournal {
    #[serde(default)]
    protocol: u64,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    items: HashMap<String, JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub kind: String,
    pub id: String,
    pub deleted_at: String,
    #[serde(default)]
    pub pushed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TombstoneStore {
    #[serde(default)]
    items: Vec<Tombstone>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled: usize,
    pub skipped: usize,
    pub conflicts: usize,
    pub recovered: usize,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report: Option<SyncReport>,
    #[serde(default)]
    pub locked_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PullReport {
    pub applied: usize,
    pub skipped: usize,
    pub missing: usize,
    pub conflicts: usize,
    pub recovered: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LeaseState {
    pub held_by_me: bool,
    pub free: bool,
    #[serde(default)]
    pub holder_device: String,
    #[serde(default)]
    pub holder_label: Option<String>,
    #[serde(default)]
    pub expires_at: u64,
}

fn runtime_cell() -> &'static Mutex<SyncRuntimeStatus> {
    static CELL: OnceLock<Mutex<SyncRuntimeStatus>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(SyncRuntimeStatus::default()))
}

pub fn runtime_status() -> SyncRuntimeStatus {
    runtime_cell().lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn is_active() -> bool {
    runtime_cell().lock().map(|s| s.active).unwrap_or(true)
}

pub fn is_profile_locked(profile_id: &str) -> bool {
    runtime_cell()
        .lock()
        .map(|status| {
            status.active
                && (status.locked_profiles.is_empty()
                    || status.locked_profiles.iter().any(|id| id == profile_id))
        })
        .unwrap_or(true)
}

fn emit_runtime(status: &SyncRuntimeStatus) {
    if let Some(app) = crate::app_handle() {
        let _ = app.emit("sync-progress", status);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_marker() -> String {
    format!("@{}", now_ms() / 1000)
}

fn sync_gate() -> &'static std::sync::Arc<AsyncMutex<()>> {
    static GATE: OnceLock<std::sync::Arc<AsyncMutex<()>>> = OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(AsyncMutex::new(())))
}

async fn acquire_sync_gate() -> Result<OwnedMutexGuard<()>> {
    sync_gate()
        .clone()
        .try_lock_owned()
        .map_err(|_| anyhow!("sync already in progress"))
}

async fn wait_for_sync_gate() -> OwnedMutexGuard<()> {
    sync_gate().clone().lock_owned().await
}

fn begin_sync(phase: &str, profile_id: Option<String>, message: &str) -> Result<()> {
    let mut status = runtime_cell().lock().map_err(|_| anyhow!("sync state unavailable"))?;
    if status.active {
        return Err(anyhow!("sync already in progress: {}", status.message));
    }
    *status = SyncRuntimeStatus {
        active: true,
        phase: phase.into(),
        locked_profiles: profile_id.clone().into_iter().collect(),
        profile_id,
        message: message.into(),
        started_at_ms: now_ms(),
        last_report: status.last_report.clone(),
    };
    emit_runtime(&status);
    Ok(())
}

fn update_sync(phase: &str, message: &str) {
    if let Ok(mut status) = runtime_cell().lock() {
        status.phase = phase.into();
        status.message = message.into();
        emit_runtime(&status);
    }
}

fn finish_sync(report: Option<SyncReport>, message: &str) {
    if let Ok(mut status) = runtime_cell().lock() {
        status.active = false;
        status.phase = if report.is_some() { "done".into() } else { "error".into() };
        status.message = message.into();
        status.locked_profiles.clear();
        if report.is_some() {
            status.last_report = report;
        }
        emit_runtime(&status);
    }
}

fn finish_success(message: &str) {
    if let Ok(mut status) = runtime_cell().lock() {
        status.active = false;
        status.phase = "done".into();
        status.message = message.into();
        status.locked_profiles.clear();
        emit_runtime(&status);
    }
}

fn clean_base_url(raw: &str) -> Result<String> {
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(anyhow!("sync server URL is empty"));
    }
    let url = reqwest::Url::parse(base).context("invalid sync server URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("sync server URL must use http or https"));
    }
    Ok(base.to_string())
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
    clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
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

fn journal_key(kind: &str, id: &str) -> String {
    format!("{kind}/{id}")
}

fn load_journal() -> Result<SyncJournal> {
    let path = store::sync_state_path()?;
    if !path.exists() {
        return Ok(SyncJournal::default());
    }
    let mut journal: SyncJournal = serde_json::from_str(&fs::read_to_string(path)?)?;
    if journal.protocol != SYNC_PROTOCOL {
        journal = SyncJournal { protocol: SYNC_PROTOCOL, ..Default::default() };
    }
    Ok(journal)
}

fn save_journal(journal: &SyncJournal) -> Result<()> {
    profile::write_atomic(&store::sync_state_path()?, &serde_json::to_vec_pretty(journal)?)
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<_, _> = map.iter().map(|(k, v)| (k.clone(), canonicalize(v))).collect();
            serde_json::to_value(ordered).unwrap_or(Value::Null)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn checksum_payload(payload: &Value, deleted: bool) -> Result<String> {
    let envelope = json!({ "deleted": deleted, "payload": canonicalize(payload) });
    let bytes = serde_json::to_vec(&envelope)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn remote_checksum(item: &SyncItem) -> Result<String> {
    checksum_payload(&item.payload, item.deleted_at.is_some())
}

fn tombstones_path() -> Result<PathBuf> {
    Ok(store::config_root()?.join("tombstones.json"))
}

fn load_tombstones() -> Result<TombstoneStore> {
    let path = tombstones_path()?;
    if !path.exists() {
        return Ok(TombstoneStore::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn save_tombstones(store_value: &TombstoneStore) -> Result<()> {
    fs::write(tombstones_path()?, serde_json::to_string_pretty(store_value)?)?;
    Ok(())
}

pub fn record_tombstone(kind: &str, id: &str) -> Result<()> {
    let mut state = load_tombstones()?;
    if let Some(existing) = state.items.iter_mut().find(|t| t.kind == kind && t.id == id) {
        existing.deleted_at = now_marker();
        existing.pushed = false;
    } else {
        state.items.push(Tombstone {
            kind: kind.into(),
            id: id.into(),
            deleted_at: now_marker(),
            pushed: false,
        });
    }
    save_tombstones(&state)
}

fn clear_tombstone(kind: &str, id: &str) -> Result<()> {
    let mut state = load_tombstones()?;
    let before = state.items.len();
    state.items.retain(|t| t.kind != kind || t.id != id);
    if before != state.items.len() {
        save_tombstones(&state)?;
    }
    Ok(())
}

fn mark_tombstone_pushed(kind: &str, id: &str) -> Result<()> {
    if kind == "profile" {
        return Ok(());
    }
    let mut state = load_tombstones()?;
    if let Some(item) = state.items.iter_mut().find(|item| item.kind == kind && item.id == id) {
        item.pushed = true;
        save_tombstones(&state)?;
    }
    Ok(())
}

fn mark_profile_tombstone_pushed(id: &str) -> Result<()> {
    let mut state = load_tombstones()?;
    if let Some(item) = state.items.iter_mut().find(|item| item.kind == "profile" && item.id == id) {
        item.pushed = true;
        save_tombstones(&state)?;
    }
    Ok(())
}

fn cloud_profile(mut stored: profile::StoredProfile) -> profile::StoredProfile {
    stored.meta.last_launched_at = None;
    stored.meta.total_runtime_ms = 0;
    stored
}

fn merge_runtime_metadata(mut remote: profile::StoredProfile, id: &str) -> profile::StoredProfile {
    remote.meta.id = id.to_string();
    if let Ok(local) = profile::load_raw(id) {
        remote.meta.last_launched_at = local.meta.last_launched_at;
        remote.meta.total_runtime_ms = local.meta.total_runtime_ms;
    }
    remote
}

fn profile_item(s: &settings::Settings, journal: &SyncJournal, profile_id: &str) -> Result<SyncItem> {
    let stored = cloud_profile(profile::load_raw(profile_id)?);
    Ok(local_item(
        s,
        journal,
        "profile",
        profile_id,
        serde_json::to_value(stored)?,
        None,
    ))
}

fn local_item(
    s: &settings::Settings,
    journal: &SyncJournal,
    kind: &str,
    id: &str,
    payload: Value,
    deleted_at: Option<String>,
) -> SyncItem {
    let revision = journal.items.get(&journal_key(kind, id)).map(|e| e.revision).unwrap_or(0);
    SyncItem {
        kind: kind.into(),
        id: id.into(),
        updated_at: now_marker(),
        deleted_at,
        device_id: s.sync_device_id.clone(),
        revision,
        checksum: None,
        payload,
    }
}

fn collect_local(s: &settings::Settings, journal: &SyncJournal) -> Result<Vec<SyncItem>> {
    let mut items = Vec::new();
    for meta in profile::list_all()? {
        items.push(profile_item(s, journal, &meta.id)?);
        if s.sync_include_cookies && !process::Tracker::shared().is_running(&meta.id) {
            let exported = cookies::export(&meta.id).unwrap_or_default();
            items.push(local_item(
                s,
                journal,
                "cookies",
                &meta.id,
                json!({ "profile_id": meta.id, "cookies": exported }),
                None,
            ));
            let bundle = export_storage_bundle(&meta.id).unwrap_or_default();
            if !bundle.is_empty() {
                items.push(local_item(
                    s,
                    journal,
                    "storage_bundle",
                    &meta.id,
                    json!({ "profile_id": meta.id, "format": "tar.gz+base64", "bytes": B64.encode(bundle) }),
                    None,
                ));
            }
        }
    }
    for entry in proxy::list()? {
        let id = entry.id.clone();
        items.push(local_item(s, journal, "proxy", &id, serde_json::to_value(entry)?, None));
    }
    for entry in fingerprints::list_all()? {
        let id = entry.id.clone();
        items.push(local_item(s, journal, "fingerprint", &id, serde_json::to_value(entry)?, None));
    }
    for tombstone in load_tombstones()?.items.into_iter().filter(|t| !t.pushed) {
        items.push(local_item(
            s,
            journal,
            &tombstone.kind,
            &tombstone.id,
            json!({}),
            Some(tombstone.deleted_at.clone()),
        ));
        if tombstone.kind == "profile" {
            for kind in ["cookies", "storage_bundle"] {
                items.push(local_item(
                    s,
                    journal,
                    kind,
                    &tombstone.id,
                    json!({}),
                    Some(tombstone.deleted_at.clone()),
                ));
            }
        }
    }
    Ok(items)
}

fn item_is_dirty(item: &SyncItem, journal: &SyncJournal) -> Result<bool> {
    let deleted = item.deleted_at.is_some();
    let checksum = checksum_payload(&item.payload, deleted)?;
    Ok(journal
        .items
        .get(&journal_key(&item.kind, &item.id))
        .map(|entry| entry.checksum != checksum || entry.deleted != deleted)
        .unwrap_or(true))
}

fn backup_conflict(item: &SyncItem) -> Result<()> {
    let safe_kind: String = item.kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let safe_id: String = item.id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let path = store::sync_conflicts_dir()?.join(format!("{}-{}-{}.json", safe_kind, safe_id, now_ms()));
    fs::write(path, serde_json::to_vec_pretty(item)?)?;
    Ok(())
}

fn update_journal(journal: &mut SyncJournal, item: &SyncItem) -> Result<()> {
    journal.protocol = SYNC_PROTOCOL;
    journal.items.insert(
        journal_key(&item.kind, &item.id),
        JournalEntry {
            revision: item.revision,
            checksum: remote_checksum(item)?,
            deleted: item.deleted_at.is_some(),
        },
    );
    Ok(())
}

fn apply_one(item: &SyncItem) -> Result<bool> {
    if item.deleted_at.is_some() {
        return match item.kind.as_str() {
            "profile" if !process::Tracker::shared().is_running(&item.id) => {
                profile::delete(&item.id)?;
                Ok(true)
            }
            "proxy" => {
                proxy::delete(&item.id)?;
                Ok(true)
            }
            "fingerprint" => {
                fingerprints::delete(&item.id)?;
                Ok(true)
            }
            _ => Ok(false),
        };
    }
    match item.kind.as_str() {
        "profile" => {
            if process::Tracker::shared().is_running(&item.id) {
                return Ok(false);
            }
            let stored: profile::StoredProfile = serde_json::from_value(item.payload.clone())?;
            let mut stored = merge_runtime_metadata(stored, &item.id);
            profile::save_synced(&mut stored)?;
            clear_tombstone("profile", &item.id)?;
            Ok(true)
        }
        "proxy" => {
            let mut entry: proxy::ProxyEntry = serde_json::from_value(item.payload.clone())?;
            entry.id = item.id.clone();
            proxy::upsert(entry)?;
            clear_tombstone("proxy", &item.id)?;
            Ok(true)
        }
        "proxies" => {
            let values: proxy::ProxyStore = serde_json::from_value(item.payload.clone())?;
            for entry in values.proxies {
                proxy::upsert(entry)?;
            }
            Ok(true)
        }
        "fingerprint" => {
            let mut entry: fingerprints::LibraryEntry = serde_json::from_value(item.payload.clone())?;
            entry.id = item.id.clone();
            let path = store::fingerprints_dir()?.join(format!("{}.json", item.id));
            fs::write(path, serde_json::to_string_pretty(&entry)?)?;
            clear_tombstone("fingerprint", &item.id)?;
            Ok(true)
        }
        "cookies" => {
            if process::Tracker::shared().is_running(&item.id) || !profile::exists(&item.id)? {
                return Ok(false);
            }
            let values: Vec<cookies::Cookie> = serde_json::from_value(
                item.payload.get("cookies").cloned().unwrap_or_else(|| json!([])),
            )?;
            cookies::import(&item.id, &values)?;
            Ok(true)
        }
        "storage_bundle" => {
            if process::Tracker::shared().is_running(&item.id) || !profile::exists(&item.id)? {
                return Ok(false);
            }
            let encoded = item.payload.get("bytes").and_then(Value::as_str).unwrap_or("");
            if encoded.is_empty() {
                return Ok(false);
            }
            import_storage_bundle(&item.id, &B64.decode(encoded)?)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn ensure_protocol(c: &reqwest::Client, s: &settings::Settings, base: &str) -> Result<()> {
    let health = auth(c.get(format!("{base}/health")), &s.sync_token)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let protocol = health.get("sync_protocol").and_then(Value::as_u64).unwrap_or(0);
    if protocol < SYNC_PROTOCOL {
        return Err(anyhow!("sync server is outdated; update it to protocol {SYNC_PROTOCOL}"));
    }
    Ok(())
}

async fn pull_changes(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    journal: &mut SyncJournal,
    report: &mut SyncReport,
) -> Result<()> {
    update_sync("pulling", "Pulling server changes");
    loop {
        let mut req = c.get(format!("{base}/sync/changes"));
        if let Some(cursor) = journal.cursor.as_ref() {
            req = req.query(&[("since", cursor)]);
        }
        let batch = auth(req, &s.sync_token)
            .send()
            .await?
            .error_for_status()?
            .json::<SyncBatch>()
            .await?;
        if batch.items.is_empty() {
            if let Some(cursor) = batch.cursor {
                journal.cursor = Some(cursor);
            }
            break;
        }
        let mut ordered = batch.items.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|item| match item.kind.as_str() {
            "profile" => 0,
            "proxy" | "proxies" | "fingerprint" => 1,
            "cookies" | "storage_bundle" => 2,
            _ => 3,
        });
        for remote in ordered {
            if matches!(remote.kind.as_str(), "cookies" | "storage_bundle") && !s.sync_include_cookies {
                update_journal(journal, remote)?;
                report.skipped += 1;
                continue;
            }
            let local = collect_local(s, journal)?
                .into_iter()
                .find(|item| item.kind == remote.kind && item.id == remote.id);
            let dirty = match local.as_ref() {
                Some(item) => item_is_dirty(item, journal)?,
                None => false,
            };
            let tracked_revision = journal
                .items
                .get(&journal_key(&remote.kind, &remote.id))
                .map(|entry| entry.revision)
                .unwrap_or(0);
            if dirty && tracked_revision > 0 && remote.revision <= tracked_revision {
                report.skipped += 1;
                continue;
            }
            if dirty && remote.revision > tracked_revision {
                if let Some(local) = local.as_ref() {
                    backup_conflict(local)?;
                }
                report.conflicts += 1;
            }
            let existed = remote.kind != "profile" || profile::exists(&remote.id)?;
            if apply_one(remote)? {
                report.pulled += 1;
                if !existed && remote.kind == "profile" {
                    report.recovered += 1;
                }
                update_journal(journal, remote)?;
            } else {
                report.skipped += 1;
            }
        }
        journal.cursor = batch.cursor;
        save_journal(journal)?;
        if batch.items.len() < 500 {
            break;
        }
    }
    Ok(())
}

async fn push_dirty(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    journal: &mut SyncJournal,
    candidates: Vec<SyncItem>,
    report: &mut SyncReport,
) -> Result<()> {
    let mut dirty = Vec::new();
    for mut item in candidates {
        if item_is_dirty(&item, journal)? {
            item.revision = journal
                .items
                .get(&journal_key(&item.kind, &item.id))
                .map(|entry| entry.revision)
                .unwrap_or(0);
            dirty.push(item);
        } else {
            report.skipped += 1;
        }
    }
    if dirty.is_empty() {
        return Ok(());
    }
    update_sync("pushing", &format!("Pushing {} changed item(s)", dirty.len()));
    let response = auth(c.post(format!("{base}/sync/push")), &s.sync_token)
        .json(&SyncBatch { items: dirty.clone(), cursor: journal.cursor.clone() })
        .send()
        .await?
        .error_for_status()?
        .json::<PushResponse>()
        .await?;
    for accepted in &response.accepted {
        update_journal(journal, accepted)?;
        mark_tombstone_pushed(&accepted.kind, &accepted.id)?;
        if accepted.deleted_at.is_some()
            && accepted.kind == "storage_bundle"
            && dirty.iter().any(|item| {
                item.id == accepted.id && item.kind == "profile" && item.deleted_at.is_some()
            })
        {
            mark_profile_tombstone_pushed(&accepted.id)?;
        }
        report.pushed += 1;
    }
    for conflict in &response.conflicts {
        if let Some(local) = dirty.iter().find(|item| item.kind == conflict.kind && item.id == conflict.id) {
            backup_conflict(local)?;
        }
        if let Some(remote) = conflict.server_item.as_ref() {
            if apply_one(remote)? {
                report.pulled += 1;
            }
            update_journal(journal, remote)?;
        }
        report.conflicts += 1;
    }
    if let Some(cursor) = response.cursor {
        journal.cursor = Some(cursor);
    }
    save_journal(journal)?;
    Ok(())
}

async fn sync_inner() -> Result<SyncReport> {
    let s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    let c = client();
    ensure_protocol(&c, &s, &base).await?;
    let mut journal = load_journal()?;
    let mut report = SyncReport::default();
    pull_changes(&c, &s, &base, &mut journal, &mut report).await?;

    let local = collect_local(&s, &journal)?;
    let mut pushable = Vec::new();
    let mut temporary_leases = Vec::new();
    for item in local {
        if matches!(item.kind.as_str(), "profile" | "cookies" | "storage_bundle") {
            if !item_is_dirty(&item, &journal)? {
                report.skipped += 1;
                continue;
            }
            if temporary_leases.iter().any(|profile_id| profile_id == &item.id) {
                pushable.push(item);
                continue;
            }
            let lease = lease_status_with(&c, &s, &base, &item.id).await?;
            if lease.held_by_me {
                pushable.push(item);
            } else if lease.free {
                let acquired = acquire_lease_with(&c, &s, &base, &item.id).await?;
                if acquired.held_by_me {
                    temporary_leases.push(item.id.clone());
                    pushable.push(item);
                } else {
                    report.skipped += 1;
                }
            } else {
                backup_conflict(&item)?;
                report.conflicts += 1;
                report.skipped += 1;
            }
        } else {
            pushable.push(item);
        }
    }
    let push_result = push_dirty(&c, &s, &base, &mut journal, pushable, &mut report).await;
    for profile_id in temporary_leases {
        release_lease_with(&c, &s, &base, &profile_id).await;
    }
    push_result?;
    report.cursor = journal.cursor.clone();
    let mut saved = s;
    saved.sync_last_cursor = journal.cursor.clone();
    settings::save(&saved)?;
    Ok(report)
}

pub fn status() -> Result<SyncStatus> {
    let s = ensure_device_id(settings::load()?)?;
    let journal = load_journal().unwrap_or_default();
    Ok(SyncStatus {
        enabled: s.sync_enabled,
        base_url: s.sync_base_url,
        device_id: s.sync_device_id,
        last_cursor: journal.cursor,
        include_cookies: s.sync_include_cookies,
    })
}

pub async fn test_connection(base_url: String, token: String) -> Result<Value> {
    let base = clean_base_url(&base_url)?;
    let response = auth(client().get(format!("{base}/health")), &token)
        .send()
        .await?
        .error_for_status()?;
    let value = response.json::<Value>().await.unwrap_or_else(|_| json!({ "ok": true }));
    let protocol = value.get("sync_protocol").and_then(Value::as_u64).unwrap_or(0);
    if protocol < SYNC_PROTOCOL {
        return Err(anyhow!("sync server is outdated; update it to protocol {SYNC_PROTOCOL}"));
    }
    Ok(value)
}

pub async fn sync_on_startup() {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() {
        return;
    }
    if let Err(error) = run_sync("startup", None, "Pulling latest sync on startup").await {
        eprintln!("[sync] startup sync failed: {error}");
    }
}

pub async fn sync_after_profile_close(profile_id: String) {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() {
        release_lease(&profile_id).await;
        return;
    }
    let _gate = wait_for_sync_gate().await;
    let label = format!("Waiting for profile {profile_id} storage flush");
    if let Err(error) = begin_sync("after_close", Some(profile_id.clone()), &label) {
        eprintln!("[sync] after close {profile_id} skipped: {error}");
        release_lease(&profile_id).await;
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    match sync_inner().await {
        Ok(report) => finish_sync(Some(report), "Sync complete"),
        Err(error) => {
            eprintln!("[sync] after close {profile_id} failed: {error}");
            finish_sync(None, &format!("Sync failed: {error}"));
        }
    }
    release_lease(&profile_id).await;
}

pub async fn sync_now() -> Result<SyncReport> {
    run_sync("manual", None, "Starting sync").await
}

async fn run_sync(phase: &str, profile_id: Option<String>, message: &str) -> Result<SyncReport> {
    let _gate = acquire_sync_gate().await?;
    begin_sync(phase, profile_id, message)?;
    match sync_inner().await {
        Ok(report) => {
            finish_sync(Some(report.clone()), "Sync complete");
            Ok(report)
        }
        Err(error) => {
            finish_sync(None, &format!("Sync failed: {error}"));
            Err(error)
        }
    }
}

async fn fetch_profile_kind(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    profile_id: &str,
    kind: &str,
    report: &mut PullReport,
) -> Result<Option<SyncItem>> {
    let response = auth(c.get(format!("{base}/sync/item/{kind}/{profile_id}")), &s.sync_token)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        report.missing += 1;
        return Ok(None);
    }
    Ok(Some(response.error_for_status()?.json::<SyncItem>().await?))
}

async fn reconcile_profile_inner(
    s: &settings::Settings,
    base: &str,
    profile_id: &str,
) -> Result<PullReport> {
    let c = client();
    ensure_protocol(&c, s, base).await?;
    let mut journal = load_journal()?;
    let mut report = PullReport::default();
    update_sync("pulling_profile", "Fetching latest profile data");

    for kind in ["profile", "cookies", "storage_bundle"] {
        if matches!(kind, "cookies" | "storage_bundle") && !s.sync_include_cookies {
            continue;
        }
        let Some(remote) = fetch_profile_kind(&c, s, base, profile_id, kind, &mut report).await? else {
            continue;
        };
        let local = if kind == "profile" && profile::exists(profile_id)? {
            Some(profile_item(s, &journal, profile_id)?)
        } else {
            collect_local(s, &journal)?
                .into_iter()
                .find(|item| item.kind == kind && item.id == profile_id)
        };
        let tracked_revision = journal
            .items
            .get(&journal_key(kind, profile_id))
            .map(|entry| entry.revision)
            .unwrap_or(0);
        let dirty = local
            .as_ref()
            .map(|item| item_is_dirty(item, &journal))
            .transpose()?
            .unwrap_or(false);
        if dirty && tracked_revision > 0 && remote.revision <= tracked_revision {
            report.skipped += 1;
            continue;
        }
        if dirty && remote.revision > tracked_revision {
            if let Some(local) = local.as_ref() {
                backup_conflict(local)?;
            }
            report.conflicts += 1;
        }
        let existed = kind != "profile" || profile::exists(profile_id)?;
        if apply_one(&remote)? {
            report.applied += 1;
            if kind == "profile" && !existed {
                report.recovered += 1;
            }
        } else {
            report.skipped += 1;
            continue;
        }
        update_journal(&mut journal, &remote)?;
    }
    if !profile::exists(profile_id)? {
        return Err(anyhow!("profile {profile_id} is unavailable locally and was not found on the sync server"));
    }
    if report.conflicts > 0 {
        save_journal(&journal)?;
        return Ok(report);
    }

    let candidates: Vec<_> = collect_local(s, &journal)?
        .into_iter()
        .filter(|item| {
            item.id == profile_id
                && matches!(item.kind.as_str(), "profile" | "cookies" | "storage_bundle")
        })
        .collect();
    let mut push_report = SyncReport::default();
    push_dirty(&c, s, base, &mut journal, candidates, &mut push_report).await?;
    report.applied += push_report.pulled;
    report.skipped += push_report.skipped;
    report.conflicts += push_report.conflicts;
    save_journal(&journal)?;
    Ok(report)
}

pub async fn pull_profile(profile_id: &str) -> Result<PullReport> {
    if process::Tracker::shared().is_running(profile_id) {
        return Ok(PullReport::default());
    }
    let s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    let _gate = acquire_sync_gate().await?;
    begin_sync("pull_before_launch", Some(profile_id.into()), "Loading latest profile from cloud")?;
    let result = reconcile_profile_inner(&s, &base, profile_id).await;
    match &result {
        Ok(report) => finish_success(&format!("Profile ready: {} applied", report.applied)),
        Err(error) => finish_sync(None, &format!("Profile cloud load failed: {error}")),
    }
    result
}

pub async fn push_profile_config(profile_id: &str) -> Result<()> {
    let loaded = settings::load()?;
    if !loaded.sync_enabled {
        return Ok(());
    }
    if process::Tracker::shared().is_running(profile_id) {
        return Ok(());
    }
    let s = ensure_device_id(loaded)?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    let _gate = acquire_sync_gate().await?;
    begin_sync("profile_push", Some(profile_id.into()), "Saving profile to cloud")?;
    let result = async {
        let c = client();
        ensure_protocol(&c, &s, &base).await?;
        let mut journal = load_journal()?;
        let mut report = SyncReport::default();
        let previous_revision = journal
            .items
            .get(&journal_key("profile", profile_id))
            .map(|entry| entry.revision)
            .unwrap_or(0);
        let remote = fetch_profile_kind(&c, &s, &base, profile_id, "profile", &mut PullReport::default()).await?;
        if let Some(remote) = remote {
            if previous_revision > 0 && remote.revision > previous_revision {
                let local = profile_item(&s, &journal, profile_id)?;
                backup_conflict(&local)?;
                apply_one(&remote)?;
                update_journal(&mut journal, &remote)?;
                save_journal(&journal)?;
                return Err(anyhow!("profile changed on another device; local copy was backed up"));
            }
            if previous_revision == 0 {
                let local = profile_item(&s, &journal, profile_id)?;
                if checksum_payload(&local.payload, false)? != remote_checksum(&remote)? {
                    backup_conflict(&local)?;
                    apply_one(&remote)?;
                    update_journal(&mut journal, &remote)?;
                    save_journal(&journal)?;
                    return Err(anyhow!("profile had no trusted server revision; local copy was backed up"));
                }
            }
            update_journal(&mut journal, &remote)?;
        } else if previous_revision > 0 {
            let local = profile_item(&s, &journal, profile_id)?;
            backup_conflict(&local)?;
            return Err(anyhow!("profile is missing on the sync server; local copy was backed up"));
        }
        let item = profile_item(&s, &journal, profile_id)?;
        let lease = lease_status_with(&c, &s, &base, profile_id).await?;
        let temporary_lease = if lease.held_by_me {
            false
        } else if lease.free {
            let acquired = acquire_lease_with(&c, &s, &base, profile_id).await?;
            if !acquired.held_by_me {
                return Err(anyhow!("profile lease could not be acquired"));
            }
            true
        } else {
            return Err(anyhow!("profile is open on another device; local change remains pending"));
        };
        let push_result = push_dirty(&c, &s, &base, &mut journal, vec![item], &mut report).await;
        if temporary_lease {
            release_lease_with(&c, &s, &base, profile_id).await;
        }
        push_result?;
        if report.conflicts > 0 {
            return Err(anyhow!("profile changed on another device; local copy was backed up"));
        }
        Ok(())
    }
    .await;
    match &result {
        Ok(()) => finish_success("Profile saved to cloud"),
        Err(error) => finish_sync(None, &format!("Profile cloud save failed: {error}")),
    }
    result
}

async fn acquire_lease_with(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    profile_id: &str,
) -> Result<LeaseState> {
    let body = json!({
        "device_id": s.sync_device_id,
        "device_label": s.sync_device_label,
        "ttl_secs": LEASE_TTL_SECS,
    });
    let response = auth(c.post(format!("{base}/lease/{profile_id}")), &s.sync_token)
        .json(&body)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        let value = response.json::<Value>().await.unwrap_or_default();
        return Ok(LeaseState {
            held_by_me: false,
            free: false,
            holder_device: value.get("holder_device").and_then(Value::as_str).unwrap_or("").into(),
            holder_label: value.get("holder_label").and_then(Value::as_str).map(String::from),
            expires_at: value.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    let value = response.error_for_status()?.json::<Value>().await.unwrap_or_default();
    Ok(LeaseState {
        held_by_me: value.get("held_by_me").and_then(Value::as_bool).unwrap_or(true),
        free: false,
        holder_device: value.get("holder_device").and_then(Value::as_str).unwrap_or("").into(),
        holder_label: value.get("holder_label").and_then(Value::as_str).map(String::from),
        expires_at: value.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
    })
}

pub async fn acquire_lease(profile_id: &str) -> Result<LeaseState> {
    let s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    acquire_lease_with(&client(), &s, &base, profile_id).await
}

async fn release_lease_with(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    profile_id: &str,
) {
    let body = json!({ "device_id": s.sync_device_id });
    let _ = auth(c.delete(format!("{base}/lease/{profile_id}")), &s.sync_token)
        .json(&body)
        .send()
        .await;
}

pub async fn release_lease(profile_id: &str) {
    let Ok(s) = configured() else { return };
    let Ok(base) = clean_base_url(s.sync_base_url.as_deref().unwrap_or("")) else { return };
    release_lease_with(&client(), &s, &base, profile_id).await;
}

async fn lease_status_with(
    c: &reqwest::Client,
    s: &settings::Settings,
    base: &str,
    profile_id: &str,
) -> Result<LeaseState> {
    let value = auth(c.get(format!("{base}/lease/{profile_id}")), &s.sync_token)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    if value.get("free").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(LeaseState { free: true, ..Default::default() });
    }
    let holder = value.get("holder_device").and_then(Value::as_str).unwrap_or("");
    Ok(LeaseState {
        held_by_me: holder == s.sync_device_id,
        free: false,
        holder_device: holder.into(),
        holder_label: value.get("holder_label").and_then(Value::as_str).map(String::from),
        expires_at: value.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
    })
}

pub async fn lease_status(profile_id: &str) -> Result<LeaseState> {
    let s = configured()?;
    let base = clean_base_url(s.sync_base_url.as_deref().unwrap_or(""))?;
    lease_status_with(&client(), &s, &base, profile_id).await
}

pub async fn sync_periodic_tick() {
    let Ok(s) = settings::load() else { return };
    if !s.sync_enabled || s.sync_base_url.as_deref().unwrap_or("").is_empty() || s.sync_token.is_empty() || is_active() {
        return;
    }
    if let Err(error) = run_sync("auto", None, "Auto-syncing").await {
        eprintln!("[sync] periodic tick failed: {error}");
    }
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
    .map(|path| root.join(path))
    .filter(|path| path.exists())
    .collect()
}

pub fn export_storage_bundle(profile_id: &str) -> Result<Vec<u8>> {
    if process::Tracker::shared().is_running(profile_id) {
        return Err(anyhow!("stop the profile before exporting storage"));
    }
    let user_data = profile::profile_user_data_dir(profile_id)?;
    let encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut archive = tar::Builder::new(encoder);
    for path in storage_paths(&user_data) {
        let relative = path.strip_prefix(&user_data).unwrap();
        if path.is_dir() {
            archive.append_dir_all(relative, &path)?;
        } else {
            archive.append_path_with_name(&path, relative)?;
        }
    }
    Ok(archive.into_inner()?.finish()?)
}

pub fn import_storage_bundle(profile_id: &str, bytes: &[u8]) -> Result<()> {
    if process::Tracker::shared().is_running(profile_id) {
        return Err(anyhow!("stop the profile before importing storage"));
    }
    let user_data = profile::profile_user_data_dir(profile_id)?;
    let decoder = GzDecoder::new(bytes);
    tar::Archive::new(decoder).unpack(user_data)?;
    Ok(())
}
