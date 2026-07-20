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
const IXB_VERSION: &str = "145.0.7632.159";
const IXB_UA_VERSION: &str = "145.0.7632.6";
const ARCHIVE_SIZE: u64 = 186_614_698;
const ARCHIVE_SHA256: &str = "4ac8b58807133da665af7971b05de5a2518536b15bd8a48db553fc3ee4ca9d0c";
const OBJECT_PREFIX: &str = "engines/ixbrowser-145/windows-x64/145.0.7632.159";
const ARCHIVE_NAME: &str = "ShardX-Chromium-145.0.7632.159-Windows-x64.zip";
const PRIMARY_BASE: &str = "https://shardx.fluxchk.biz.id";
const FALLBACK_BASE: &str = "https://pub-c075bb957f2f4a26b4bebaee35b0af0f.r2.dev";
const EXPECTED_ENTRYPOINT: &str = "ShardX-Chromium-145.0.7632.159-Windows-x64/chrome.exe";

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
}

fn runtime_root() -> Result<PathBuf> {
    Ok(crate::store::config_root()?
        .join("runtimes")
        .join(profile::ENGINE_IXBROWSER_145)
        .join(IXB_VERSION))
}

fn downloaded_binary_path() -> Result<PathBuf> {
    Ok(runtime_root()?.join(EXPECTED_ENTRYPOINT))
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

#[tauri::command]
pub async fn ixbrowser_status() -> Result<EngineStatus, String> {
    let manual_path = settings::load().ok().and_then(|s| s.ixbrowser_145_path);
    let binary_path = resolve_binary_optional();
    Ok(EngineStatus {
        installed: binary_path.is_some(),
        version: IXB_VERSION.into(),
        size_bytes: binary_path.as_ref().and_then(|p| p.parent()).map(dir_size),
        binary_path,
        manual_path,
    })
}

fn resolve_binary_optional() -> Option<PathBuf> {
    let settings = settings::load().ok()?;
    if let Some(path) = settings.ixbrowser_145_path.filter(|p| !p.trim().is_empty()) {
        let path = PathBuf::from(path);
        return validate_bundle(&path).ok().map(|_| path);
    }
    if let Ok(path) = downloaded_binary_path() {
        if validate_bundle(&path).is_ok() {
            return Some(path);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let path = default_binary_path();
        if validate_bundle(&path).is_ok() {
            return Some(path);
        }
    }
    None
}

pub fn resolve_binary() -> Result<PathBuf> {
    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("Chromium 145 compatibility is available on Windows only");

    #[cfg(target_os = "windows")]
    {
        if let Some(configured) = settings::load()?
            .ixbrowser_145_path
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
        {
            validate_bundle(&configured).with_context(|| {
                format!("configured Chromium 145 path is invalid: {}", configured.display())
            })?;
            return Ok(configured);
        }
        if let Ok(downloaded) = downloaded_binary_path() {
            if validate_bundle(&downloaded).is_ok() {
                return Ok(downloaded);
            }
        }
        let detected = default_binary_path();
        if validate_bundle(&detected).is_ok() {
            return Ok(detected);
        }
        anyhow::bail!("Chromium 145 compatibility is not installed. Download it in Settings or choose a local chrome.exe.")
    }
}

#[cfg(target_os = "windows")]
fn default_binary_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("ixBrowser-Resources")
        .join("chrome")
        .join("145-0104")
        .join("chrome.exe")
}

fn validate_bundle(binary: &Path) -> Result<()> {
    let dir = binary.parent().context("Chromium 145 binary has no parent directory")?;
    let required = [
        dir.join(format!("{IXB_VERSION}.manifest")),
        dir.join("chrome.dll"),
        dir.join("resources.pak"),
        dir.join("icudtl.dat"),
        dir.join("Locales").join("en-US.pak"),
    ];
    if !binary.is_file() || required.iter().any(|p| !p.is_file()) {
        anyhow::bail!(
            "{} is not a complete ixBrowser Chromium {IXB_VERSION} bundle",
            dir.display()
        );
    }
    Ok(())
}

fn validate_manifest(manifest: &RemoteManifest) -> Result<()> {
    if manifest.engine != profile::ENGINE_IXBROWSER_145
        || manifest.platform != "windows-x64"
        || manifest.version != IXB_VERSION
        || manifest.archive_size != ARCHIVE_SIZE
        || !manifest.archive_sha256.eq_ignore_ascii_case(ARCHIVE_SHA256)
        || manifest.entrypoint.replace('\\', "/") != EXPECTED_ENTRYPOINT
    {
        anyhow::bail!("Chromium 145 manifest does not match the pinned runtime");
    }
    Ok(())
}

fn emit_progress(window: &Window, phase: &str, received: u64, total: u64, source: &str) {
    let percent = if total == 0 { 0 } else { ((received.saturating_mul(100) / total).min(100)) as u8 };
    let _ = window.emit("ixbrowser:progress", InstallProgress {
        phase: phase.into(), received, total, percent, source: source.into(),
    });
}

async fn fetch_manifest(url: &str) -> Result<RemoteManifest> {
    let response = reqwest::Client::new().get(url).send().await?.error_for_status()?;
    let manifest = response.json::<RemoteManifest>().await?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

async fn download_archive(window: &Window, archive_url: &str, part: &Path, source: &str) -> Result<()> {
    let mut response = reqwest::Client::new()
        .get(archive_url)
        .send()
        .await?
        .error_for_status()?;
    if let Some(length) = response.content_length() {
        if length != ARCHIVE_SIZE {
            anyhow::bail!("Chromium 145 archive length is {length}, expected {ARCHIVE_SIZE}");
        }
    }
    let mut file = tokio::fs::File::create(part).await?;
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        emit_progress(window, "downloading", received, ARCHIVE_SIZE, source);
    }
    file.flush().await?;
    if received != ARCHIVE_SIZE {
        anyhow::bail!("Chromium 145 download is {received} bytes, expected {ARCHIVE_SIZE}");
    }
    Ok(())
}

fn verify_sha256(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 { break; }
        hasher.update(&buf[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != ARCHIVE_SHA256 {
        anyhow::bail!("Chromium 145 SHA-256 mismatch: {actual}");
    }
    Ok(())
}

fn extract_archive(zip_path: &Path, staging: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    std::fs::create_dir_all(staging)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry.enclosed_name().context("unsafe path in Chromium 145 archive")?;
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

#[tauri::command]
pub async fn ixbrowser_install(window: Window, force: bool) -> Result<EngineStatus, String> {
    #[cfg(not(target_os = "windows"))]
    return Err("Chromium 145 compatibility is available on Windows only".into());

    #[cfg(target_os = "windows")]
    {
        let final_root = runtime_root().map_err(|e| e.to_string())?;
        if !force {
            if let Ok(binary) = downloaded_binary_path() {
                if validate_bundle(&binary).is_ok() {
                    return ixbrowser_status().await;
                }
            }
        }
        let parent = final_root.parent().ok_or("invalid Chromium 145 runtime path")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let part = parent.join(format!("{IXB_VERSION}.zip.part"));
        let staging = parent.join(format!(".{IXB_VERSION}.staging-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_dir_all(&staging);

        let sources = [(PRIMARY_BASE, "custom-domain"), (FALLBACK_BASE, "r2.dev-fallback")];
        let mut last_error = String::new();
        let mut downloaded = false;
        for (base, source) in sources {
            let manifest_url = format!("{base}/{OBJECT_PREFIX}/manifest.json");
            let archive_url = format!("{base}/{OBJECT_PREFIX}/{ARCHIVE_NAME}");
            emit_progress(&window, "manifest", 0, ARCHIVE_SIZE, source);
            let attempt = async {
                let _manifest = fetch_manifest(&manifest_url).await?;
                download_archive(&window, &archive_url, &part, source).await?;
                verify_sha256(&part)?;
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
            return Err(format!("Chromium 145 download failed: {last_error}"));
        }

        emit_progress(&window, "extracting", ARCHIVE_SIZE, ARCHIVE_SIZE, "local");
        extract_archive(&part, &staging).map_err(|e| e.to_string())?;
        let staged_binary = staging.join(EXPECTED_ENTRYPOINT);
        validate_bundle(&staged_binary).map_err(|e| e.to_string())?;
        if final_root.exists() {
            std::fs::remove_dir_all(&final_root).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&staging, &final_root).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&part);
        emit_progress(&window, "done", ARCHIVE_SIZE, ARCHIVE_SIZE, "local");
        let status = ixbrowser_status().await?;
        let _ = window.emit("ixbrowser:done", status.clone());
        Ok(status)
    }
}

pub fn build_launch_config(
    profile_id: &str,
    raw: &Map<String, Value>,
    udd: &Path,
    geo: Option<&proxy::GeoInfo>,
    public_ip: Option<&str>,
) -> Result<LaunchConfig> {
    let binary = resolve_binary()?;
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
    let timezone = raw
        .get("timezone")
        .and_then(Value::as_str)
        .filter(|v| *v != "auto")
        .or_else(|| geo.map(|g| g.timezone.as_str()).filter(|v| !v.is_empty()))
        .unwrap_or("UTC");

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
        "TTSEngines": raw.pointer("/speech/voices").cloned().unwrap_or_else(|| json!([])),
        "RestoreLastSession": false,
        "AutomationControlled": false,
        "PersistExtensions": "extensionCenter",
        "MediaDevices": media_devices(raw),
        "DeviceName": device_name(profile_id),
        "ProductType": device_name(profile_id),
        "MinWindowWidth": 100,
        "JsHeapSizeLimit": raw.pointer("/memory/heap_size_limit").and_then(Value::as_u64).unwrap_or(4_294_967_296).to_string()
    });

    let mut dynamic = Map::new();
    dynamic.insert("BlockList".into(), json!({ "udp": "", "tcp": "" }));
    dynamic.insert("TimeZone".into(), json!({ "id": timezone }));
    if let Some(g) = geo {
        if g.latitude != 0.0 && g.longitude != 0.0 {
            dynamic.insert(
                "Geoposition".into(),
                json!({ "accuracy": 50, "latitude": g.latitude, "longitude": g.longitude }),
            );
        }
    }
    if let Some(ip) = public_ip.filter(|v| !v.is_empty()) {
        dynamic.insert("PublicIP".into(), Value::String(ip.into()));
        dynamic.insert("WebRTC".into(), json!({ "publicIP": ip }));
    }

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
    std::fs::write(&static_path, encode_json(&static_config)?)?;
    std::fs::write(&dynamic_path, encode_json(&Value::Object(dynamic))?)?;
    std::fs::write(&webgl_path, serde_json::to_vec(&webgl_config)?)?;

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
        "mark": format!("shardx-{profile_id}"),
        "DarkMode": false,
        "keep-bgwin-visible": 1
    });

    let ua = format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{IXB_UA_VERSION} Safari/537.36"
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

    Ok(LaunchConfig { binary, args })
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
