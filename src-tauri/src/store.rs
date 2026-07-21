// Persistent storage layout under the user's config dir:
//   $CONFIG/shardx-launcher/
//     profiles/                   ← fingerprint profile JSON files
//     proxies.json                ← saved proxy list
//     proxy-cache-keys.json       ← hashed proxy config for GeoIP cache validity
//     user-data/<profile-id>/     ← per-profile user-data-dir for ShardX
//     settings.json               ← global app settings

use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn config_root() -> Result<PathBuf> {
    let base = dirs::config_dir().context("OS config dir unavailable")?;
    let root = base.join("shardx-launcher");
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

pub fn profiles_dir() -> Result<PathBuf> {
    let p = config_root()?.join("profiles");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn fingerprints_dir() -> Result<PathBuf> {
    let p = config_root()?.join("fingerprints");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

/// Cached Widevine CDM, seeded from a host Chrome install (or
/// downloaded from the project's git LFS bucket for end users).  When
/// present, every freshly-created profile's user-data-dir gets a
/// pre-warmed `WidevineCdm/` copy so the browser doesn't sit waiting
/// on the component updater the first time a DRM page (Netflix /
/// Spotify / etc.) loads.
pub fn widevine_cache_dir() -> Result<PathBuf> {
    Ok(config_root()?.join("widevine-cdm"))
}

pub fn user_data_root() -> Result<PathBuf> {
    let p = config_root()?.join("user-data");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn proxies_path() -> Result<PathBuf> {
    Ok(config_root()?.join("proxies.json"))
}

pub fn proxy_cache_keys_path() -> Result<PathBuf> {
    Ok(config_root()?.join("proxy-cache-keys.json"))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(config_root()?.join("settings.json"))
}

/// ProxyShard billing-API config (Bearer key). Kept in its own file so the
/// Settings page (which round-trips the whole Settings struct) can never
/// clobber the saved key.
pub fn psapi_path() -> Result<PathBuf> {
    Ok(config_root()?.join("psapi.json"))
}
