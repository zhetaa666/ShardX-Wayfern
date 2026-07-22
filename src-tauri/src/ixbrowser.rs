use crate::{profile, proxy, settings};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Window};
use tokio::io::AsyncWriteExt;

const STANDARD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const IXB_ALPHABET: &[u8; 64] =
    b"hTy1bfRJz4nLPcBCO7WtmNIaGvVeul5Zo8kq32UxrYw_-0gsjp96SDFXQiEMKdHA";
const PRIMARY_BASE: &str = "https://shardx.fluxchk.biz.id";
const FALLBACK_BASE: &str = "https://pub-c075bb957f2f4a26b4bebaee35b0af0f.r2.dev";

struct EngineSpec {
    engine: &'static str,
    label: &'static str,
    version: &'static str,
    ua_version: &'static str,
    archive_size: u64,
    archive_sha256: &'static str,
    object_prefix: &'static str,
    archive_name: &'static str,
    entrypoint: &'static str,
    default_dir: &'static str,
    progress_event: &'static str,
    done_event: &'static str,
}

const IXBROWSER_145: EngineSpec = EngineSpec {
    engine: profile::ENGINE_IXBROWSER_145,
    label: "Chromium 145",
    version: "145.0.7632.159",
    ua_version: "145.0.7632.6",
    archive_size: 186_614_698,
    archive_sha256: "4ac8b58807133da665af7971b05de5a2518536b15bd8a48db553fc3ee4ca9d0c",
    object_prefix: "engines/ixbrowser-145/windows-x64/145.0.7632.159",
    archive_name: "ShardX-Chromium-145.0.7632.159-Windows-x64.zip",
    entrypoint: "ShardX-Chromium-145.0.7632.159-Windows-x64/chrome.exe",
    default_dir: "145-0104",
    progress_event: "ixbrowser:progress",
    done_event: "ixbrowser:done",
};

const IXBROWSER_148: EngineSpec = EngineSpec {
    engine: profile::ENGINE_IXBROWSER_148,
    label: "Chromium 148",
    version: "148.0.7778.167",
    ua_version: "148.0.7778.59",
    archive_size: 193_635_357,
    archive_sha256: "ae688a5bcdaa4dafe53b974b6546780a622c0b3e19ac5e8dd42eca6eac7d7df3",
    object_prefix: "engines/ixbrowser-148/windows-x64/148.0.7778.167",
    archive_name: "ShardX-Chromium-148.0.7778.167-Windows-x64.zip",
    entrypoint: "ShardX-Chromium-148.0.7778.167-Windows-x64/chrome.exe",
    default_dir: "148-0005",
    progress_event: "ixbrowser148:progress",
    done_event: "ixbrowser148:done",
};

fn engine_spec(engine: &str) -> Result<&'static EngineSpec> {
    match profile::normalize_browser_engine(engine) {
        profile::ENGINE_IXBROWSER_145 => Ok(&IXBROWSER_145),
        profile::ENGINE_IXBROWSER_148 => Ok(&IXBROWSER_148),
        _ => anyhow::bail!("unsupported ixBrowser engine: {engine}"),
    }
}

#[derive(Serialize, Clone)]
pub struct EngineStatus {
    pub installed: bool,
    pub version: String,
    pub binary_path: Option<PathBuf>,
    pub size_bytes: Option<u64>,
    pub manual_path: Option<String>,
}

#[derive(Serialize, Clone)]
struct InstallProgress {
    phase: String,
    received: u64,
    total: u64,
    percent: u8,
    source: String,
}

#[derive(Deserialize)]
struct RemoteManifest {
    engine: String,
    platform: String,
    version: String,
    archive_sha256: String,
    archive_size: u64,
    entrypoint: String,
}

pub struct LaunchConfig {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub disabled_features: Vec<String>,
}

fn runtime_root(spec: &EngineSpec) -> Result<PathBuf> {
    Ok(crate::store::config_root()?
        .join("runtimes")
        .join(spec.engine)
        .join(spec.version))
}

fn downloaded_binary_path(spec: &EngineSpec) -> Result<PathBuf> {
    Ok(runtime_root(spec)?.join(spec.entrypoint))
}

fn manual_path(settings: &settings::Settings, spec: &EngineSpec) -> Option<String> {
    match spec.engine {
        profile::ENGINE_IXBROWSER_145 => settings.ixbrowser_145_path.clone(),
        profile::ENGINE_IXBROWSER_148 => settings.ixbrowser_148_path.clone(),
        _ => None,
    }
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            entry.metadata().map(|m| {
                if m.is_dir() { dir_size(&entry.path()) } else { m.len() }
            }).unwrap_or(0)
        })
        .sum()
}

async fn status(spec: &'static EngineSpec) -> Result<EngineStatus, String> {
    let configured = settings::load().ok();
    let manual_path = configured.as_ref().and_then(|s| manual_path(s, spec));
    let binary_path = resolve_binary_optional(spec);
    Ok(EngineStatus {
        installed: binary_path.is_some(),
        version: spec.version.into(),
        size_bytes: binary_path.as_ref().and_then(|p| p.parent()).map(dir_size),
        binary_path,
        manual_path,
    })
}

#[tauri::command]
pub async fn ixbrowser_status() -> Result<EngineStatus, String> {
    status(&IXBROWSER_145).await
}

#[tauri::command]
pub async fn ixbrowser_148_status() -> Result<EngineStatus, String> {
    status(&IXBROWSER_148).await
}

fn resolve_binary_optional(spec: &EngineSpec) -> Option<PathBuf> {
    let settings = settings::load().ok()?;
    if let Some(path) = manual_path(&settings, spec).filter(|p| !p.trim().is_empty()) {
        let path = PathBuf::from(path);
        return validate_bundle(&path, spec).ok().map(|_| path);
    }
    if let Ok(path) = downloaded_binary_path(spec) {
        if validate_bundle(&path, spec).is_ok() {
            return Some(path);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let path = default_binary_path(spec);
        if validate_bundle(&path, spec).is_ok() {
            return Some(path);
        }
    }
    None
}

pub fn resolve_binary(engine: &str) -> Result<PathBuf> {
    let spec = engine_spec(engine)?;
    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("{} compatibility is available on Windows only", spec.label);

    #[cfg(target_os = "windows")]
    {
        if let Some(configured) = manual_path(&settings::load()?, spec)
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
        {
            validate_bundle(&configured, spec).with_context(|| {
                format!("configured {} path is invalid: {}", spec.label, configured.display())
            })?;
            return Ok(configured);
        }
        if let Ok(downloaded) = downloaded_binary_path(spec) {
            if validate_bundle(&downloaded, spec).is_ok() {
                return Ok(downloaded);
            }
        }
        let detected = default_binary_path(spec);
        if validate_bundle(&detected, spec).is_ok() {
            return Ok(detected);
        }
        anyhow::bail!(
            "{} compatibility is not installed. Download it in Settings or choose a local chrome.exe.",
            spec.label
        )
    }
}

#[cfg(target_os = "windows")]
fn default_binary_path(spec: &EngineSpec) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("ixBrowser-Resources")
        .join("chrome")
        .join(spec.default_dir)
        .join("chrome.exe")
}

fn validate_bundle(binary: &Path, spec: &EngineSpec) -> Result<()> {
    let dir = binary
        .parent()
        .with_context(|| format!("{} binary has no parent directory", spec.label))?;
    let required = [
        dir.join(format!("{}.manifest", spec.version)),
        dir.join("chrome.dll"),
        dir.join("resources.pak"),
        dir.join("icudtl.dat"),
        dir.join("Locales").join("en-US.pak"),
    ];
    if !binary.is_file() || required.iter().any(|p| !p.is_file()) {
        anyhow::bail!(
            "{} is not a complete ixBrowser {} bundle",
            dir.display(),
            spec.version
        );
    }
    Ok(())
}

fn validate_manifest(manifest: &RemoteManifest, spec: &EngineSpec) -> Result<()> {
    if manifest.engine != spec.engine
        || manifest.platform != "windows-x64"
        || manifest.version != spec.version
        || manifest.archive_size != spec.archive_size
        || !manifest.archive_sha256.eq_ignore_ascii_case(spec.archive_sha256)
        || manifest.entrypoint.replace('\\', "/") != spec.entrypoint
    {
        anyhow::bail!("{} manifest does not match the pinned runtime", spec.label);
    }
    Ok(())
}

fn emit_progress(
    window: &Window,
    spec: &EngineSpec,
    phase: &str,
    received: u64,
    total: u64,
    source: &str,
) {
    let percent = if total == 0 {
        0
    } else {
        ((received.saturating_mul(100) / total).min(100)) as u8
    };
    let _ = window.emit(spec.progress_event, InstallProgress {
        phase: phase.into(), received, total, percent, source: source.into(),
    });
}

async fn fetch_manifest(url: &str, spec: &EngineSpec) -> Result<RemoteManifest> {
    let response = reqwest::Client::new().get(url).send().await?.error_for_status()?;
    let manifest = response.json::<RemoteManifest>().await?;
    validate_manifest(&manifest, spec)?;
    Ok(manifest)
}

async fn download_archive(
    window: &Window,
    spec: &EngineSpec,
    archive_url: &str,
    part: &Path,
    source: &str,
) -> Result<()> {
    let mut response = reqwest::Client::new()
        .get(archive_url)
        .send()
        .await?
        .error_for_status()?;
    if let Some(length) = response.content_length() {
        if length != spec.archive_size {
            anyhow::bail!(
                "{} archive length is {length}, expected {}",
                spec.label,
                spec.archive_size
            );
        }
    }
    let mut file = tokio::fs::File::create(part).await?;
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        emit_progress(window, spec, "downloading", received, spec.archive_size, source);
    }
    file.flush().await?;
    if received != spec.archive_size {
        anyhow::bail!(
            "{} download is {received} bytes, expected {}",
            spec.label,
            spec.archive_size
        );
    }
    Ok(())
}

fn verify_sha256(path: &Path, spec: &EngineSpec) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 { break; }
        hasher.update(&buf[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != spec.archive_sha256 {
        anyhow::bail!("{} SHA-256 mismatch: {actual}", spec.label);
    }
    Ok(())
}

fn extract_archive(zip_path: &Path, staging: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    std::fs::create_dir_all(staging)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry.enclosed_name().context("unsafe path in ixBrowser archive")?;
        let output = staging.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
        } else {
            if let Some(parent) = output.parent() { std::fs::create_dir_all(parent)?; }
            let mut target = std::fs::File::create(&output)?;
            std::io::copy(&mut entry, &mut target)?;
        }
    }
    Ok(())
}

async fn install(window: Window, force: bool, spec: &'static EngineSpec) -> Result<EngineStatus, String> {
    #[cfg(not(target_os = "windows"))]
    return Err(format!("{} compatibility is available on Windows only", spec.label));

    #[cfg(target_os = "windows")]
    {
        let final_root = runtime_root(spec).map_err(|e| e.to_string())?;
        if !force {
            if let Ok(binary) = downloaded_binary_path(spec) {
                if validate_bundle(&binary, spec).is_ok() {
                    return status(spec).await;
                }
            }
        }
        let parent = final_root
            .parent()
            .ok_or_else(|| format!("invalid {} runtime path", spec.label))?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let part = parent.join(format!("{}.zip.part", spec.version));
        let staging = parent.join(format!(".{}.staging-{}", spec.version, uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_dir_all(&staging);

        let sources = [(PRIMARY_BASE, "custom-domain"), (FALLBACK_BASE, "r2.dev-fallback")];
        let mut last_error = String::new();
        let mut downloaded = false;
        for (base, source) in sources {
            let manifest_url = format!("{base}/{}/manifest.json", spec.object_prefix);
            let archive_url = format!("{base}/{}/{}", spec.object_prefix, spec.archive_name);
            emit_progress(&window, spec, "manifest", 0, spec.archive_size, source);
            let attempt = async {
                let _manifest = fetch_manifest(&manifest_url, spec).await?;
                download_archive(&window, spec, &archive_url, &part, source).await?;
                verify_sha256(&part, spec)?;
                Ok::<(), anyhow::Error>(())
            }.await;
            match attempt {
                Ok(()) => { downloaded = true; break; }
                Err(error) => {
                    last_error = format!("{source}: {error}");
                    let _ = std::fs::remove_file(&part);
                }
            }
        }
        if !downloaded {
            return Err(format!("{} download failed: {last_error}", spec.label));
        }

        emit_progress(&window, spec, "extracting", spec.archive_size, spec.archive_size, "local");
        extract_archive(&part, &staging).map_err(|e| e.to_string())?;
        let staged_binary = staging.join(spec.entrypoint);
        validate_bundle(&staged_binary, spec).map_err(|e| e.to_string())?;
        if final_root.exists() {
            std::fs::remove_dir_all(&final_root).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&staging, &final_root).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&part);
        emit_progress(&window, spec, "done", spec.archive_size, spec.archive_size, "local");
        let status = status(spec).await?;
        let _ = window.emit(spec.done_event, status.clone());
        Ok(status)
    }
}

#[tauri::command]
pub async fn ixbrowser_install(window: Window, force: bool) -> Result<EngineStatus, String> {
    install(window, force, &IXBROWSER_145).await
}

#[tauri::command]
pub async fn ixbrowser_148_install(window: Window, force: bool) -> Result<EngineStatus, String> {
    install(window, force, &IXBROWSER_148).await
}

pub fn build_launch_config(
    engine: &str,
    profile_id: &str,
    profile_name: &str,
    raw: &Map<String, Value>,
    udd: &Path,
    geo: Option<&proxy::GeoInfo>,
    host_public_ips: &[String],
    webrtc_public_ip: Option<&str>,
    timezone: &str,
) -> Result<LaunchConfig> {
    let spec = engine_spec(engine)?;
    let binary = resolve_binary(engine)?;
    let nav = object(raw, "navigator");
    let screen = object(raw, "screen");
    let window = object(raw, "window");
    let webgl = object(raw, "webgl");

    let locale = nav
        .and_then(|v| v.get("language"))
        .and_then(Value::as_str)
        .filter(|v| *v != "auto")
        .unwrap_or("en-US");
    let languages = nav
        .and_then(|v| v.get("languages"))
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| locale_languages(locale));
    let accept_language = nav
        .and_then(|v| v.get("accept_language"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or(&languages);
    let static_config = json!({
        "MaxTouchPoints": value_u64(nav, "max_touch_points", 0),
        "FoceSafeBrowsing": true,
        "UserAgentMetadata": {
            "platform": "Windows",
            "platformVersion": nav.and_then(|v| v.get("platform_version")).and_then(Value::as_str).unwrap_or("19.0"),
            "architecture": "x86",
            "bitness": "64",
            "model": "",
            "mobile": false,
            "wow64": false
        },
        "TTSEngines": raw.get("speech").and_then(|v| v.get("voices")).cloned().unwrap_or_else(|| json!([])),
        "RestoreLastSession": false,
        "AutomationControlled": false,
        "PersistExtensions": "extensionCenter",
        "MediaDevices": media_devices(raw),
        "DeviceName": device_name(profile_id),
        "ProductType": device_name(profile_id),
        "MinWindowWidth": 100,
        "JsHeapSizeLimit": raw.get("memory").and_then(|v| v.get("heap_size_limit")).and_then(Value::as_u64).unwrap_or(4_294_967_296).to_string()
    });

    let dynamic = build_dynamic_config(geo, host_public_ips, webrtc_public_ip, timezone)?;

    let webgl_config = json!({
        "UNMASKED_VENDOR_WEBGL": webgl.and_then(|v| v.get("vendor")).and_then(Value::as_str).unwrap_or("Google Inc."),
        "UNMASKED_RENDERER_WEBGL": webgl.and_then(|v| v.get("renderer")).and_then(Value::as_str).unwrap_or("ANGLE"),
        "SUPPORTED_EXTENSIONS": []
    });

    let config_dir = udd.join("ixbrowser-config");
    std::fs::create_dir_all(&config_dir)?;
    let static_path = config_dir.join("static.config");
    let dynamic_path = config_dir.join("dynamic.config");
    let webgl_path = config_dir.join("webgl.json");
    write_atomic(&static_path, encode_json(&static_config)?.as_bytes())?;
    write_atomic(&dynamic_path, encode_json(&dynamic)?.as_bytes())?;
    write_atomic(&webgl_path, &serde_json::to_vec(&webgl_config)?)?;
    let decoded_dynamic = decode_json(&std::fs::read_to_string(&dynamic_path)?)?;
    validate_dynamic_config(
        &decoded_dynamic,
        host_public_ips,
        webrtc_public_ip,
        timezone,
    )?;

    let marks = Marks::new(profile_id);
    let extended = json!({
        "UserId": 8,
        "StartTime": unix_now(),
        "FlashPluginSetting": "block",
        "DisableBackgroundMode": true,
        "HardwareConcurrency": value_u64(nav, "hardware_concurrency", 8),
        "DeviceMemory": value_u64(nav, "device_memory", 8),
        "ForceProcessExit": true,
        "WebGLMark": marks.webgl,
        "AllowScanPorts": "0",
        "GeolocationSetting": if geo.is_some() { "allow" } else { "ask" },
        "Geoposition": geo.and_then(|g| (g.latitude != 0.0 && g.longitude != 0.0).then(|| format!("{},{},50", g.latitude, g.longitude))),
        "Langs": languages,
        "AcceptLang": accept_language,
        "Platform": "Win32",
        "CanvasMark": marks.canvas,
        "AudioFp": marks.audio,
        "ClientRectFp": marks.client_rects,
        "StaticConfig": static_path,
        "DynamicConfig": dynamic_path,
        "WebGLFP": webgl_path,
        "mark": profile_name.trim(),
        "DarkMode": false,
        "keep-bgwin-visible": 1
    });

    let ua = format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{} Safari/537.36",
        spec.ua_version
    );
    let renderer = webgl
        .and_then(|v| v.get("renderer"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut args = vec![
        "--force-color-profile=srgb".into(),
        "--metrics-recording-only".into(),
        "--no-first-run".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        "--no-default-browser-check".into(),
        "--disable-background-mode".into(),
        "--disable-extension-welcome-page".into(),
        "--autoplay-policy=no-user-gesture-required".into(),
        "--protected-enablechromeversion=1".into(),
        "--enable-unsafe-swiftshader".into(),
        "--protected-gems=2147483649".into(),
        "--js-flags=--ignore_debugger".into(),
        format!("--protected-webgpu={}", webgpu_family(renderer)),
        "--disable-popup-blocking".into(),
        "--hide-crash-restore-bubble".into(),
        format!("--user-agent={ua}"),
        format!("--lang={locale}"),
        "--no-sandbox".into(),
        "--disable-setuid-sandbox".into(),
        format!("--extended-parameters={}", encode_json(&extended)?),
    ];
    if let (Some(width), Some(height)) = (
        window.and_then(|v| v.get("outer_width")).and_then(Value::as_u64).or_else(|| screen.and_then(|v| v.get("width")).and_then(Value::as_u64)),
        window.and_then(|v| v.get("outer_height")).and_then(Value::as_u64).or_else(|| screen.and_then(|v| v.get("avail_height")).and_then(Value::as_u64)),
    ) {
        args.push(format!("--window-size={width},{height}"));
    }
    let disabled_features = if spec.engine == profile::ENGINE_IXBROWSER_148 {
        args.push("--window-position=0,0".into());
        [
            "HttpsUpgrades",
            "HttpsFirstModeV2ForEngagedSites",
            "HttpsFirstBalancedMode",
            "HttpsFirstBalancedModeAutoEnable",
            "EnableFingerprintingProtectionFilter",
            "FlashDeprecationWarning",
            "EnablePasswordsAccountStorage",
            "RendererCodeIntegrity",
            "CanvasNoise",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    } else {
        Vec::new()
    };

    Ok(LaunchConfig { binary, args, disabled_features })
}

fn build_dynamic_config(
    geo: Option<&proxy::GeoInfo>,
    host_public_ips: &[String],
    webrtc_public_ip: Option<&str>,
    timezone: &str,
) -> Result<Value> {
    let mut dynamic = Map::new();
    dynamic.insert(
        "BlockList".into(),
        json!({ "Version": "2", "Domains": [] }),
    );
    dynamic.insert("TimeZone".into(), json!(timezone));
    if let Some(g) = geo {
        if g.latitude != 0.0 && g.longitude != 0.0 {
            dynamic.insert(
                "Geoposition".into(),
                Value::String(format!("{},{},50", g.latitude, g.longitude)),
            );
        }
    }
    if let Some(replacement) = webrtc_public_ip.filter(|value| !value.is_empty()) {
        if host_public_ips.is_empty() {
            anyhow::bail!(
                "ixBrowser WebRTC replacement requires the host public IP; test or rebind the proxy first"
            );
        }
        dynamic.insert(
            "PublicIP".into(),
            Value::String(host_public_ips.join(",")),
        );
        dynamic.insert(
            "WebRTCLocalAddress".into(),
            Value::String(replacement.into()),
        );
        dynamic.insert(
            "WebRTCAddress".into(),
            Value::String(replacement.into()),
        );
    }
    Ok(Value::Object(dynamic))
}

fn validate_dynamic_config(
    dynamic: &Value,
    host_public_ips: &[String],
    webrtc_public_ip: Option<&str>,
    timezone: &str,
) -> Result<()> {
    if dynamic.get("TimeZone").and_then(Value::as_str) != Some(timezone) {
        anyhow::bail!("ixBrowser dynamic timezone validation failed");
    }
    if let Some(replacement) = webrtc_public_ip.filter(|value| !value.is_empty()) {
        let expected_sources = host_public_ips.join(",");
        let public_ip = dynamic.get("PublicIP").and_then(Value::as_str);
        let local = dynamic.get("WebRTCLocalAddress").and_then(Value::as_str);
        let public = dynamic.get("WebRTCAddress").and_then(Value::as_str);
        if public_ip != Some(expected_sources.as_str())
            || local != Some(replacement)
            || public != Some(replacement)
        {
            anyhow::bail!("ixBrowser dynamic WebRTC replacement validation failed");
        }
    }
    Ok(())
}

fn encode_json(value: &Value) -> Result<String> {
    let json = serde_json::to_vec(value)?;
    let standard = STANDARD.encode(json);
    let mut translated = Vec::with_capacity(standard.len());
    for byte in standard.bytes() {
        translated.push(match STANDARD_ALPHABET.iter().position(|c| *c == byte) {
            Some(index) => IXB_ALPHABET[index],
            None => byte,
        });
    }
    String::from_utf8(translated).context("ixBrowser Base64 output was not UTF-8")
}

fn decode_json(encoded: &str) -> Result<Value> {
    let mut translated = Vec::with_capacity(encoded.len());
    for byte in encoded.trim().bytes() {
        translated.push(match IXB_ALPHABET.iter().position(|c| *c == byte) {
            Some(index) => STANDARD_ALPHABET[index],
            None => byte,
        });
    }
    let bytes = STANDARD.decode(translated)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn object<'a>(raw: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    raw.get(key).and_then(Value::as_object)
}

fn value_u64(object: Option<&Map<String, Value>>, key: &str, fallback: u64) -> u64 {
    object.and_then(|v| v.get(key)).and_then(Value::as_u64).unwrap_or(fallback)
}

fn locale_languages(locale: &str) -> String {
    let base = locale.split('-').next().unwrap_or(locale);
    if locale == "en-US" {
        "en-US,en".into()
    } else {
        format!("{locale},{base},en-US,en")
    }
}

fn media_devices(raw: &Map<String, Value>) -> Value {
    let media = object(raw, "media_devices");
    let mut out = Vec::new();
    let counts = [
        ("audio_input_count", "audioinput", "Microphone Array (High Definition Audio)"),
        ("video_input_count", "videoinput", "Integrated Camera"),
        ("audio_output_count", "audiooutput", "Speakers (High Definition Audio)"),
    ];
    for (key, kind, label) in counts {
        let count = value_u64(media, key, 0).min(4);
        for index in 0..count {
            let suffix = if index == 0 { String::new() } else { format!(" {}", index + 1) };
            out.push(json!({ "kind": kind, "label": format!("{label}{suffix}") }));
        }
    }
    Value::Array(out)
}

fn device_name(profile_id: &str) -> String {
    let suffix: String = profile_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect();
    format!("DESKTOP-{}", suffix.to_ascii_uppercase())
}

fn webgpu_family(renderer: &str) -> &'static str {
    let value = renderer.to_ascii_lowercase();
    if value.contains("nvidia") || value.contains("geforce") || value.contains("quadro") {
        "nvidia,ampere"
    } else if value.contains("amd") || value.contains("radeon") {
        "amd,gcn-5"
    } else {
        "intel,gen-12"
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0)
}

struct Marks {
    canvas: i32,
    webgl: i32,
    audio: i32,
    client_rects: i32,
}

impl Marks {
    fn new(profile_id: &str) -> Self {
        Self {
            canvas: positive_mark(profile_id, "canvas"),
            webgl: positive_mark(profile_id, "webgl"),
            audio: signed_mark(profile_id, "audio"),
            client_rects: signed_mark(profile_id, "client_rects"),
        }
    }
}

fn hash(profile_id: &str, slot: &str) -> u32 {
    let mut value = 0x811c9dc5u32;
    for byte in format!("{profile_id}:{slot}").bytes() {
        value ^= byte as u32;
        value = value.wrapping_mul(0x01000193);
    }
    value
}

fn positive_mark(profile_id: &str, slot: &str) -> i32 {
    (hash(profile_id, slot) % 9_999 + 1) as i32
}

fn signed_mark(profile_id: &str, slot: &str) -> i32 {
    (hash(profile_id, slot) % 19_999) as i32 - 9_999
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_timezone_round_trips_as_string() {
        let value = json!({
            "BlockList": { "Version": "2", "Domains": [] },
            "TimeZone": "America/Detroit"
        });
        let encoded = encode_json(&value).unwrap();
        let decoded = decode_json(&encoded).unwrap();
        assert_eq!(decoded.get("TimeZone").and_then(Value::as_str), Some("America/Detroit"));
    }

    #[test]
    fn dynamic_webrtc_maps_host_sources_to_proxy_replacement() {
        let sources = vec![
            "168.144.42.200".to_string(),
            "2001:db8::25".to_string(),
        ];
        let dynamic = build_dynamic_config(
            None,
            &sources,
            Some("212.102.46.41"),
            "America/Los_Angeles",
        )
        .unwrap();

        assert_eq!(
            dynamic.get("PublicIP").and_then(Value::as_str),
            Some("168.144.42.200,2001:db8::25")
        );
        assert_eq!(
            dynamic.get("WebRTCAddress").and_then(Value::as_str),
            Some("212.102.46.41")
        );
        assert_eq!(
            dynamic.get("WebRTCLocalAddress").and_then(Value::as_str),
            Some("212.102.46.41")
        );
        validate_dynamic_config(
            &dynamic,
            &sources,
            Some("212.102.46.41"),
            "America/Los_Angeles",
        )
        .unwrap();
    }

    #[test]
    fn dynamic_webrtc_rejects_missing_host_source() {
        let error = build_dynamic_config(
            None,
            &[],
            Some("212.102.46.41"),
            "America/Los_Angeles",
        )
        .unwrap_err();

        assert!(error.to_string().contains("host public IP"));
    }
}
