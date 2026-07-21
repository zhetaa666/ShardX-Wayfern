use crate::store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// Absolute path to the ShardX executable.
    pub browser_path: Option<String>,
    /// Local/private ixBrowser Chromium 145 executable (Windows only).
    #[serde(default)]
    pub ixbrowser_145_path: Option<String>,
    /// Theme: "dark" (default) or "light".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Geo-IP checker provider used by the proxy "Test" button.
    /// One of "ixbrowser.com" | "ip-api.com" | "ipapi.co" | "ipwho.is".
    #[serde(default)]
    pub geo_checker: Option<String>,
    /// "fingerprint" (use the screen from the bound fingerprint) or
    /// "real" (let ShardX use the host's real screen).
    #[serde(default)]
    pub screen_resolution_mode: Option<String>,
    /// Hide the launcher to the system tray on close instead of quitting.
    #[serde(default = "default_minimize_to_tray")]
    pub minimize_to_tray: bool,

    // ---- Local automation HTTP API (axum + JWT bearer) ----
    /// Whether the local API server listens on 127.0.0.1:`api_port`.
    #[serde(default = "default_api_enabled")]
    pub api_enabled: bool,
    /// Port the API binds on 127.0.0.1.
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    /// HS256 signing key for API JWTs.  Auto-generated on first run
    /// (see `ensure_secret`); rotating it invalidates issued tokens.
    #[serde(default)]
    pub api_secret: String,
    /// Keep headed CDP renderers active when their windows are minimized or occluded.
    #[serde(default = "default_true")]
    pub api_disable_background_throttling: bool,

    /// Self-hosted sync client. Server URL + bearer token are user-provided.
    #[serde(default)]
    pub sync_enabled: bool,
    #[serde(default)]
    pub sync_base_url: Option<String>,
    #[serde(default)]
    pub sync_token: String,
    #[serde(default)]
    pub sync_device_id: String,
    #[serde(default)]
    pub sync_last_cursor: Option<String>,
    #[serde(default)]
    pub sync_include_cookies: bool,
    /// Pull the latest profile bundle from the server on Start before
    /// spawning (premium "pull-before-Start" behaviour). Default on.
    #[serde(default = "default_true")]
    pub sync_pull_on_start: bool,
    /// Auto-push interval in seconds while idle (0 = off). Default 300.
    #[serde(default = "default_auto_push_secs")]
    pub sync_auto_push_secs: u32,
    /// Human label for this device, shown in other devices' "In use" badge.
    #[serde(default)]
    pub sync_device_label: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_auto_push_secs() -> u32 {
    300
}

fn default_theme() -> String {
    "dark".into()
}

fn default_minimize_to_tray() -> bool {
    true
}

fn default_api_enabled() -> bool {
    true
}

fn default_api_port() -> u16 {
    40325
}

pub fn load() -> Result<Settings> {
    let path = store::settings_path()?;
    if !path.exists() {
        return Ok(Settings {
            browser_path: None,
            ixbrowser_145_path: None,
            theme: default_theme(),
            geo_checker: Some("ixbrowser.com".into()),
            screen_resolution_mode: Some("fingerprint".into()),
            minimize_to_tray: default_minimize_to_tray(),
            api_enabled: default_api_enabled(),
            api_port: default_api_port(),
            api_secret: String::new(),
            api_disable_background_throttling: true,
            sync_enabled: false,
            sync_base_url: None,
            sync_token: String::new(),
            sync_device_id: String::new(),
            sync_last_cursor: None,
            sync_include_cookies: false,
            sync_pull_on_start: true,
            sync_auto_push_secs: default_auto_push_secs(),
            sync_device_label: None,
        });
    }
    let body = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

/// Load settings, generating + persisting the API JWT secret if it's
/// still empty.  Call once at startup before the server reads it.
pub fn ensure_secret() -> Result<Settings> {
    let mut s = load()?;
    if s.api_secret.is_empty() {
        s.api_secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        save(&s)?;
    }
    Ok(s)
}

pub fn save(s: &Settings) -> Result<()> {
    let body = serde_json::to_string_pretty(s)?;
    fs::write(store::settings_path()?, body)?;
    Ok(())
}
