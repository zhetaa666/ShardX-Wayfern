use crate::{
    process::{self, Tracker},
    profile, proxy, settings, store,
};
use anyhow::{Context, Result};
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Launch result: OS pid plus CDP endpoint when remote-debugging is on.
pub struct LaunchOutcome {
    pub pid: u32,
    pub cdp: Option<process::CdpInfo>,
}

/// Resolve the ShardX executable from settings, runtime cache, or dev guess.
pub fn resolve_binary() -> Result<PathBuf> {
    if let Some(p) = settings::load()?.browser_path {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    if let Ok(pb) = crate::runtime::binary_path() {
        if pb.exists() {
            return Ok(pb);
        }
    }
    #[cfg(target_os = "macos")]
    let guess = "/Users/kritos/Documents/GitHub/ShardXBrowser/build/src/out/Release_GN_arm64/ShardX.app/Contents/MacOS/ShardX";
    #[cfg(target_os = "windows")]
    let guess = "C:\\Program Files\\ShardX\\ShardX.exe";
    #[cfg(target_os = "linux")]
    let guess = "/opt/shardx/shardx";
    let pb = PathBuf::from(guess);
    if pb.exists() {
        return Ok(pb);
    }
    anyhow::bail!("ShardX browser not installed yet — open Settings to download, or configure Browser path manually")
}

fn remove_session_artifacts(user_data_dir: &Path) {
    let default = user_data_dir.join("Default");
    let sessions = default.join("Sessions");
    if sessions.exists() {
        let _ = std::fs::remove_dir_all(sessions);
    }
    for name in ["Current Session", "Current Tabs", "Last Session", "Last Tabs"] {
        let _ = std::fs::remove_file(default.join(name));
    }
}

fn configure_fresh_startup(user_data_dir: &Path) {
    remove_session_artifacts(user_data_dir);
    let preferences = user_data_dir.join("Default").join("Preferences");
    let Ok(body) = std::fs::read_to_string(&preferences) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
        return;
    };
    let Some(root) = value.as_object_mut() else {
        return;
    };
    let session = root
        .entry("session".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(session) = session.as_object_mut() {
        session.insert("restore_on_startup".into(), serde_json::json!(5));
    }
    if let Ok(updated) = serde_json::to_vec(&value) {
        let _ = profile::write_atomic(&preferences, &updated);
    }
}

pub async fn launch_profile(
    profile_id: &str,
    enable_cdp: bool,
    headless: bool,
) -> Result<LaunchOutcome> {
    if Tracker::shared().is_running(profile_id) {
        anyhow::bail!("profile is already running");
    }
    let stored = profile::load_raw(profile_id)?;
    let engine = profile::normalize_browser_engine(&stored.meta.browser_engine);
    let udd = profile::engine_user_data_dir(profile_id, engine)?;
    process::kill_stale_user_data_processes(&udd);
    configure_fresh_startup(&udd);
    for marker in ["DevToolsActivePort", "SingletonCookie", "SingletonLock", "SingletonSocket"] {
        let _ = std::fs::remove_file(udd.join(marker));
    }

    // Stored proxy by id, else ephemeral inline (quick profiles, not in store).
    let bound_proxy: Option<proxy::ProxyEntry> = stored
        .meta
        .proxy_id
        .as_deref()
        .and_then(|pid| proxy::get(pid).ok().flatten())
        .or_else(|| stored.meta.inline_proxy.clone());

    // UDP capability from the proxy's last full test (cached). No live probe
    // at launch: a per-launch UDP_ASSOCIATE both delays the spawn and, on
    // typical SOCKS5 providers, degrades the relay for sessions that are
    // already browsing — noticeable when several profiles run concurrently.
    let proxy_udp_ok = bound_proxy
        .as_ref()
        .map(|p| {
            matches!(p.kind, proxy::ProxyKind::Socks5)
                && proxy::latest_matching_test(p).and_then(|s| s.udp_ms).is_some()
        })
        .unwrap_or(false);

    // Strip `_meta` wrapper and resolve "auto" sentinels before serialising.
    let mut raw = stored.config.clone();
    raw.remove("_meta");
    let cached_geo = bound_proxy
        .as_ref()
        .and_then(proxy::latest_matching_test)
        .and_then(snapshot_geo);
    let auto_geo = resolve_auto_fields(&mut raw, bound_proxy.as_ref(), cached_geo.as_ref());
    let effective_geo = auto_geo.as_ref().or(cached_geo.as_ref());
    let resolved_timezone = raw
        .get("timezone")
        .and_then(serde_json::Value::as_str)
        .filter(|v| *v != "auto" && !v.is_empty())
        .unwrap_or("UTC")
        .to_string();
    let json = serde_json::to_string(&raw).context("serialize profile")?;
    let proxy_public_ip = effective_geo
        .map(|g| g.ip.as_str())
        .filter(|ip| !ip.is_empty());
    let host_public_ips = if profile::is_ixbrowser_engine(engine) && bound_proxy.is_some() {
        host_source_ips()
    } else {
        Vec::new()
    };
    if profile::is_ixbrowser_engine(engine)
        && bound_proxy.is_some()
        && (proxy_public_ip.is_none() || host_public_ips.is_empty())
    {
        anyhow::bail!(
            "ixBrowser WebRTC replacement data is incomplete; test or rebind the proxy before launch"
        );
    }
    let profile_name = stored
        .config
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("untitled");

    let (bin, mut engine_args, mut disabled_features) = if profile::is_ixbrowser_engine(engine) {
        let config = crate::ixbrowser::build_launch_config(
            engine,
            profile_id,
            profile_name,
            &raw,
            &udd,
            effective_geo,
            &host_public_ips,
            proxy_public_ip,
            &resolved_timezone,
        )?;
        (config.binary, config.args, config.disabled_features)
    } else {
        // Pass fingerprint by file path — inline JSON overflows Windows' 32767-char CreateProcess limit.
        let fp_file = udd.join("fingerprint.json");
        std::fs::write(&fp_file, &json).context("write fingerprint.json")?;
        (
            resolve_binary()?,
            vec![format!("--fingerprint-profile={}", fp_file.display())],
            Vec::new(),
        )
    };

    // Pre-warm Widevine CDM to avoid first-DRM-page component-updater stall.
    if let Err(e) = install_widevine(&udd) {
        eprintln!("[launcher] widevine pre-warm skipped: {e}");
    }

    let mut cmd = tokio::process::Command::new(&bin);
    if profile::is_ixbrowser_engine(engine) {
        if let Some(parent) = bin.parent() {
            cmd.current_dir(parent);
        }
    }
    cmd.args(engine_args.drain(..));
    cmd.arg(format!("--user-data-dir={}", udd.display()));
    cmd.arg("--no-first-run");

    // Chromium only honors the last --disable-features argument.
    let webgpu_present = raw
        .get("webgpu")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let keep_cdp_active = enable_cdp
        && !headless
        && settings::load()?.api_disable_background_throttling;
    if engine != profile::ENGINE_IXBROWSER_145 && !webgpu_present {
        disabled_features.push("WebGPU".into());
    }
    if engine != profile::ENGINE_IXBROWSER_145 && keep_cdp_active {
        disabled_features.push("CalculateNativeWinOcclusion".into());
    }
    if !disabled_features.is_empty() {
        cmd.arg(format!("--disable-features={}", disabled_features.join(",")));
    }
    if keep_cdp_active {
        cmd.arg("--disable-background-timer-throttling");
        cmd.arg("--disable-backgrounding-occluded-windows");
        cmd.arg("--disable-renderer-backgrounding");
    }

    // Interactive launches start on Chromium's default tab without reopening
    // prior windows; cookies and site storage remain in the user-data-dir.
    if !headless && !enable_cdp {
        cmd.arg("--hide-crash-restore-bubble");
    }

    if let Some(p) = bound_proxy.as_ref() {
        let scheme = match p.kind {
            proxy::ProxyKind::Socks5 => "socks5",
            proxy::ProxyKind::Http => "http",
            proxy::ProxyKind::Https => "https",
        };
        eprintln!(
            "[launcher] profile={profile_id} proxy_id={} proxy={scheme}://{}:{} timezone={resolved_timezone}",
            p.id, p.host, p.port
        );
        cmd.arg(format!("--proxy-server={}", p.to_proxy_server_arg()));

        // QUIC always OFF behind a proxy.  Chromium routes HTTP/3 through the
        // SOCKS5 UDP relay, which commercial proxies rate-limit or half-break;
        // the QUIC/TCP race then stalls every page load and gets dramatically
        // worse with several concurrent profiles.  Paid antidetect browsers
        // ship the same policy: proxy bound → no QUIC.
        cmd.arg("--disable-quic");
        eprintln!("[launcher] QUIC disabled (proxy bound: {})", p.host);
    }

    // WebRTC IP policy: block / tcp_only / auto (auto = relay if UDP, else tcp_only).
    let webrtc_mode = raw
        .get("webrtc")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    let latest = bound_proxy
        .as_ref()
        .and_then(proxy::latest_matching_test);
    // Public IP for ICE-candidate spoofing: reuse the geo fetched during
    // auto-field resolution when available, else the cached test snapshot.
    // Never a live lookup here — launch already cost one geo round-trip at
    // most, and hammering the proxy at spawn slows concurrent sessions.
    let proxy_public_ip: Option<String> = if bound_proxy.is_some() {
        effective_geo
            .map(|g| g.ip.clone())
            .filter(|ip| !ip.is_empty())
            .or_else(|| latest.as_ref().map(|s| s.ip.clone()).filter(|ip| !ip.is_empty()))
    } else {
        None
    };
    match webrtc_mode {
        "block" => {
            cmd.arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
            if !profile::is_ixbrowser_engine(engine) {
                cmd.arg("--shardx-webrtc-policy=block");
            }
            eprintln!("[launcher] WebRTC blocked (servers stripped, relay-only, UDP off)");
        }
        "tcp_only" => {
            cmd.arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
            if !profile::is_ixbrowser_engine(engine) {
                cmd.arg("--shardx-webrtc-policy=tcp_only");
                if let Some(ip) = proxy_public_ip.as_deref() {
                    cmd.arg(format!("--shardx-webrtc-public-ip={ip}"));
                }
            }
            eprintln!("[launcher] WebRTC: TCP-only (servers stripped, mDNS host only, UDP off)");
        }
        _ => {
            if bound_proxy.is_none() {
                // No proxy bound — let WebRTC use the host network natively
                // (real IP shows in ICE candidates, which is what the user wants
                // when they explicitly didn't bind a proxy).
                eprintln!("[launcher] WebRTC auto -> native (no proxy bound)");
            } else if !proxy_udp_ok {
                cmd.arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
                if !profile::is_ixbrowser_engine(engine) {
                    cmd.arg("--shardx-webrtc-policy=tcp_only");
                    if let Some(ip) = proxy_public_ip.as_deref() {
                        cmd.arg(format!("--shardx-webrtc-public-ip={ip}"));
                    }
                }
                eprintln!("[launcher] WebRTC auto -> TCP-only (no proxied UDP available)");
            } else {
                eprintln!("[launcher] WebRTC auto -> through proxy UDP relay");
            }
        }
    }

    // Screen resolution mode: presence-only switch to use host monitor.
    let s = settings::load()?;
    if s.screen_resolution_mode.as_deref() == Some("real")
        && !profile::is_ixbrowser_engine(engine)
    {
        cmd.arg("--shardx-real-screen");
    }

    // CDP: port=0 makes Chrome pick free port and write DevToolsActivePort.
    if enable_cdp {
        let _ = std::fs::remove_file(udd.join("DevToolsActivePort"));
        cmd.arg("--remote-debugging-port=0");
        cmd.arg("--remote-allow-origins=*");
    }

    if headless {
        cmd.arg("--headless=new");
    }

    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // 0x08000000 = CREATE_NO_WINDOW — suppress the brief console flash
        // when a Tauri GUI app spawns the engine binary.
        cmd.creation_flags(0x08000000);
    }
    let child = cmd.spawn().context("spawn ShardX")?;
    let pid = Tracker::shared().track(profile_id.to_string(), child, stored.meta.temporary);

    profile::touch_launched(profile_id, None)?;

    let cdp = if enable_cdp {
        match read_devtools_endpoint(&udd).await {
            Some(c) => {
                eprintln!("[launcher] CDP ready for {profile_id}: {}", c.web_socket_debugger_url);
                Tracker::shared().set_cdp(profile_id, c.clone());
                Some(c)
            }
            None => {
                eprintln!("[launcher] CDP: DevToolsActivePort not found within timeout");
                None
            }
        }
    } else {
        None
    };

    Ok(LaunchOutcome { pid, cdp })
}

/// Poll `<udd>/DevToolsActivePort` for ~6s; line 1 = port, line 2 = ws path.
async fn read_devtools_endpoint(udd: &Path) -> Option<process::CdpInfo> {
    let file = udd.join("DevToolsActivePort");
    for _ in 0..60 {
        if let Ok(txt) = std::fs::read_to_string(&file) {
            let mut lines = txt.lines();
            if let (Some(port_s), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port_s.trim().parse::<u16>() {
                    return Some(process::CdpInfo {
                        port,
                        http_url: format!("http://127.0.0.1:{port}"),
                        web_socket_debugger_url: format!(
                            "ws://127.0.0.1:{port}{}",
                            path.trim()
                        ),
                    });
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

fn host_source_ips() -> Vec<String> {
    let mut addresses = proxy::cached_host_public_ips();
    for (bind, target) in [
        ("0.0.0.0:0", "192.0.2.1:9"),
        ("[::]:0", "[2001:db8::1]:9"),
    ] {
        let Ok(socket) = UdpSocket::bind(bind) else {
            continue;
        };
        let Ok(target) = target.parse::<std::net::SocketAddr>() else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        let Ok(local) = socket.local_addr() else {
            continue;
        };
        let ip = local.ip();
        if is_public_source_ip(ip) {
            let value = ip.to_string();
            if !addresses.iter().any(|existing| existing == &value) {
                addresses.push(value);
            }
        }
    }
    addresses
}

fn is_public_source_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified())
        }
        IpAddr::V6(ip) => !(ip.is_loopback() || ip.is_unicast_link_local() || ip.is_unspecified()),
    }
}

fn snapshot_geo(snap: proxy::TestSnapshot) -> Option<proxy::GeoInfo> {
    if snap.country_code.is_empty() && snap.timezone.is_empty() && snap.ip.is_empty() {
        return None;
    }
    Some(proxy::GeoInfo {
        ip: snap.ip,
        country: snap.country,
        country_code: snap.country_code,
        region: snap.region,
        city: snap.city,
        isp: snap.isp,
        timezone: snap.timezone,
        latitude: snap.latitude,
        longitude: snap.longitude,
        provider: snap.provider,
    })
}

/// Resolve auto fields from the cached proxy test. Normal launch never probes
/// the proxy or a geo provider; untested proxies fall back to their country tag.
fn resolve_auto_fields(
    cfg: &mut serde_json::Map<String, serde_json::Value>,
    proxy_opt: Option<&proxy::ProxyEntry>,
    cached_geo: Option<&proxy::GeoInfo>,
) -> Option<proxy::GeoInfo> {
    let want_tz_auto = cfg.get("timezone").and_then(|v| v.as_str()) == Some("auto");
    let want_lang_auto = cfg
        .get("navigator")
        .and_then(|n| n.get("language"))
        .and_then(|v| v.as_str())
        == Some("auto");
    let want_geo_auto = matches!(
        cfg.get("geolocation").and_then(|g| g.get("mode")).and_then(|v| v.as_str()),
        Some("auto")
    );

    if !(want_tz_auto || want_lang_auto || want_geo_auto) {
        return None;
    }

    eprintln!(
        "[launcher] resolving auto fields (tz={} lang={} geo={} proxy={})",
        want_tz_auto,
        want_lang_auto,
        want_geo_auto,
        proxy_opt.map(|p| format!("{}:{}", p.host, p.port)).unwrap_or_else(|| "(direct)".into()),
    );

    // ---- geo source ----
    let (source, geo): (&str, Option<proxy::GeoInfo>) = match (cached_geo, proxy_opt) {
        (Some(g), _) => ("cached-snapshot", Some(g.clone())),
        (None, Some(p)) if !p.country.is_empty() => (
            "country-tag",
            Some(proxy::GeoInfo {
                ip: String::new(),
                country: String::new(),
                country_code: p.country.clone(),
                region: String::new(),
                city: String::new(),
                isp: String::new(),
                timezone: String::new(),
                latitude: 0.0,
                longitude: 0.0,
                provider: String::new(),
            }),
        ),
        _ => ("host", None),
    };

    let host_warn = || {
        if proxy_opt.is_some() {
            eprintln!(
                "[launcher] WARNING: proxy is bound but every geo source failed; \
                 using the LAUNCHER HOST's TZ/locale.  This will leak your real \
                 timezone — re-test the proxy or set the timezone manually."
            );
        }
    };

    // ---- concrete tz/locale/lat/lng ----
    let (resolved_tz, resolved_locale, resolved_lat, resolved_lng) = match geo {
        Some(ref g) => {
            let tz = if !g.timezone.is_empty() {
                g.timezone.clone()
            } else {
                proxy::country_to_timezone(&g.country_code).to_string()
            };
            let locale = proxy::country_to_locale(&g.country_code).to_string();
            let lat = if g.latitude != 0.0 { Some(g.latitude) } else { None };
            let lng = if g.longitude != 0.0 { Some(g.longitude) } else { None };
            (tz, locale, lat, lng)
        }
        None => {
            host_warn();
            (
                host_timezone().unwrap_or_else(|| "UTC".into()),
                host_locale().unwrap_or_else(|| "en-US".into()),
                None,
                None,
            )
        }
    };

    eprintln!(
        "[launcher] resolved tz={resolved_tz} locale={resolved_locale} (source={source})"
    );

    if want_tz_auto {
        cfg.insert("timezone".into(), serde_json::Value::String(resolved_tz.clone()));
    }

    if want_lang_auto {
        let base = resolved_locale.split('-').next().unwrap_or(&resolved_locale).to_string();
        let accept = if resolved_locale == "en-US" {
            "en-US,en;q=0.9".to_string()
        } else {
            format!("{resolved_locale},{base};q=0.9,en-US;q=0.8,en;q=0.7")
        };
        let languages = if resolved_locale == "en-US" {
            vec![
                serde_json::Value::String("en-US".into()),
                serde_json::Value::String("en".into()),
            ]
        } else {
            vec![
                serde_json::Value::String(resolved_locale.clone()),
                serde_json::Value::String(base),
                serde_json::Value::String("en-US".into()),
                serde_json::Value::String("en".into()),
            ]
        };
        if let Some(nav) = cfg.get_mut("navigator").and_then(|v| v.as_object_mut()) {
            nav.insert("language".into(), serde_json::Value::String(resolved_locale.clone()));
            nav.insert("accept_language".into(), serde_json::Value::String(accept));
            nav.insert("languages".into(), serde_json::Value::Array(languages));
        }
        // Always overwrite icu_locale so it matches resolved navigator.language.
        cfg.insert("icu_locale".into(), serde_json::Value::String(resolved_locale));
    }

    if want_geo_auto {
        if let (Some(lat), Some(lng)) = (resolved_lat, resolved_lng) {
            cfg.insert(
                "geolocation".into(),
                serde_json::json!({
                    "mode": "manual",
                    "latitude": lat,
                    "longitude": lng,
                    "accuracy": 50.0,
                }),
            );
        } else {
            cfg.remove("geolocation");
        }
    }

    geo
}

/// Copy cached Widevine CDM into `<udd>/WidevineCdm/<version>/` (versioned layout
/// required by Chromium's DefaultComponentInstaller). No-op if cache absent.
fn install_widevine(udd: &Path) -> Result<()> {
    let src = store::widevine_cache_dir()?;
    if !src.exists() {
        anyhow::bail!("cache dir absent ({})", src.display());
    }
    let manifest_path = src.join("manifest.json");
    if !manifest_path.exists() {
        anyhow::bail!("cache missing manifest.json — re-seed from a real Chrome");
    }
    let manifest_text = std::fs::read_to_string(&manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .context("parse widevine manifest.json")?;
    let version = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("widevine manifest missing `version`"))?;

    let widevine_root = udd.join("WidevineCdm");
    let versioned = widevine_root.join(version);
    if versioned.exists() {
        return Ok(());
    }
    // Clean up any stale flat layout from older launcher versions.
    let flat_manifest = widevine_root.join("manifest.json");
    if flat_manifest.exists() {
        for stray in ["manifest.json", "LICENSE", "_platform_specific"] {
            let p = widevine_root.join(stray);
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else if p.exists() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    copy_dir_recursive(&src, &versioned).with_context(|| {
        format!("copy {} → {}", src.display(), versioned.display())
    })?;
    // Chromium reads this single-line marker on startup.
    std::fs::write(
        widevine_root.join("latest-component-updated-version"),
        version,
    )?;
    eprintln!("[launcher] widevine pre-warmed: {}", versioned.display());
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_symlink() {
            // Resolve symlinks so dst tree stays portable across hosts.
            let target = std::fs::read_link(&from)?;
            let resolved = if target.is_absolute() { target } else { from.parent().unwrap().join(target) };
            if resolved.is_dir() {
                copy_dir_recursive(&resolved, &to)?;
            } else {
                std::fs::copy(&resolved, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Read host TZ from /etc/localtime symlink, fall back to $TZ.
fn host_timezone() -> Option<String> {
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let path = target.to_string_lossy().into_owned();
        for prefix in ["/usr/share/zoneinfo/", "/var/db/timezone/zoneinfo/"] {
            if let Some(tz) = path.strip_prefix(prefix) {
                return Some(tz.to_string());
            }
        }
    }
    std::env::var("TZ").ok().filter(|s| !s.is_empty())
}

/// Extract BCP-47 locale from $LANG/$LC_ALL ("en_US.UTF-8" → "en-US").
fn host_locale() -> Option<String> {
    for var in ["LANG", "LC_ALL", "LC_MESSAGES"] {
        if let Ok(v) = std::env::var(var) {
            let stripped = v.split('.').next().unwrap_or("").replace('_', "-");
            if stripped.contains('-') {
                return Some(stripped);
            }
        }
    }
    None
}
