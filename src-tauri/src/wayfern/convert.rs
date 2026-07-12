//! Wayfern 75-field flat fingerprint → ShardX FingerprintConfig (nested).
//!
//! Port of `wayfern-fresh/convert.mjs`. The output JSON matches ShardX's
//! `LibraryEntry.payload` schema so it can be fed straight into
//! `fingerprints::import`.

use serde_json::{json, Value};

/// Convert a Wayfern flat fingerprint to a ShardX FingerprintConfig.
/// `label` becomes the `name` field; when None, a derived name is used.
pub fn wayfern_to_shardx(w: &Value, label: Option<&str>) -> Value {
    let user_agent = w.get("userAgent").and_then(|v| v.as_str()).unwrap_or("");
    let platform = w.get("platform").and_then(|v| v.as_str()).unwrap_or("");
    let ua = parse_ua(user_agent);
    let canvas_hex = w
        .get("canvasNoiseSeed")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let canvas_seed = hex_to_u32(canvas_hex);
    let webgl_seed = fnv1a(&format!("{canvas_hex}::webgl"));
    let audio_seed = fnv1a(&format!("{canvas_hex}::audio"));
    let client_rects_seed = fnv1a(&format!("{canvas_hex}::client_rects"));
    let sensors_seed = fnv1a(&format!("{canvas_hex}::sensors"));
    let fonts_seed = fnv1a(&format!("{canvas_hex}::fonts"));

    let name = label
        .map(String::from)
        .unwrap_or_else(|| format!("Wayfern {} · {platform}", ua.major));
    let notes = format!("Generated via Wayfern CDP (Chrome {})", ua.full);

    let platform_version = w
        .get("platformVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("19.0.0")
        .to_string();

    let ch_platform = ch_platform(platform, user_agent);
    let ch_architecture = ch_architecture(user_agent);
    let ch_bitness = ch_bitness(user_agent);
    let max_touch_points = w
        .get("maxTouchPoints")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let languages: Vec<Value> = w
        .get("languages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| {
            let lang = w
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("en-US");
            vec![json!(lang), json!("en")]
        });
    let accept_language = accept_language(w);

    let geolocation = match (
        w.get("latitude").and_then(|v| v.as_f64()),
        w.get("longitude").and_then(|v| v.as_f64()),
    ) {
        (Some(lat), Some(lon)) => json!({
            "mode": "manual",
            "latitude": lat,
            "longitude": lon,
            "accuracy": 50,
        }),
        _ => json!({ "mode": "auto" }),
    };

    json!({
        "name": name,
        "notes": notes,
        "seed": 0,
        "timezone": w.get("timezone").and_then(|v| v.as_str()).unwrap_or("auto"),
        "icu_locale": w.get("language").cloned().unwrap_or(Value::Null),
        "webrtc": "auto",
        "navigator": {
            "user_agent": user_agent,
            "platform": platform,
            "platform_version": platform_version,
            "hardware_concurrency": w.get("hardwareConcurrency").cloned().unwrap_or(json!(8)),
            "device_memory": w.get("deviceMemory").cloned().unwrap_or(json!(8)),
            "language": w.get("language").cloned().unwrap_or(json!("en-US")),
            "accept_language": accept_language,
            "languages": languages,
            "do_not_track": w.get("doNotTrack").cloned().unwrap_or(Value::Null),
        },
        "client_hints": {
            "platform": ch_platform,
            "platform_version": w.get("platformVersion").cloned().unwrap_or(json!("19.0.0")),
            "architecture": ch_architecture,
            "bitness": ch_bitness,
            "mobile": max_touch_points > 0,
            "brand_version": ua.major.clone(),
            "brand_full_version": ua.full.clone(),
            "chrome_build": ua.build.clone(),
            "chrome_patch": ua.patch.clone(),
        },
        "screen": {
            "width": w.get("screenWidth").cloned().unwrap_or(json!(1920)),
            "height": w.get("screenHeight").cloned().unwrap_or(json!(1080)),
            "avail_width": w.get("screenAvailWidth").cloned().unwrap_or(json!(1920)),
            "avail_height": w.get("screenAvailHeight").cloned().unwrap_or(json!(1040)),
            "color_depth": w.get("screenColorDepth").cloned().unwrap_or(json!(24)),
            "device_pixel_ratio": w.get("devicePixelRatio").cloned().unwrap_or(json!(1.0)),
            "color_gamut": color_gamut(w),
        },
        "audio": {
            "sample_rate": w.get("audioSampleRate").cloned().unwrap_or(json!(48000)),
            "channel_count": w.get("audioMaxChannelCount").cloned().unwrap_or(json!(2)),
        },
        "webgl": webgl_block(w),
        "noise": {
            "canvas":       { "enabled": false, "seed": canvas_seed },
            "webgl":        { "enabled": false, "seed": webgl_seed, "intensity": 0 },
            "audio":        { "enabled": false, "seed": audio_seed, "intensity": 0 },
            "client_rects": { "enabled": false, "seed": client_rects_seed, "max_offset": 0 },
            "sensors":      { "enabled": false, "seed": sensors_seed },
            "fonts":        { "enabled": false, "seed": fonts_seed },
        },
        "geolocation": geolocation,
        "media_devices": {
            "audio_input_count": 1,
            "audio_output_count": 1,
            "video_input_count": if max_touch_points > 0 { 1 } else { 0 },
        },
        "blocked_ports": [],
        "_wayfern_extras": {
            "canvas_noise_seed_hex": canvas_hex,
            "audio_sample_rate": w.get("audioSampleRate").cloned().unwrap_or(Value::Null),
            "audio_max_channel_count": w.get("audioMaxChannelCount").cloned().unwrap_or(Value::Null),
            "voices_json": w.get("voices").cloned().unwrap_or(Value::Null),
            "fonts_list_json": w.get("fonts").cloned().unwrap_or(Value::Null),
            "mime_types_json": w.get("mimeTypes").cloned().unwrap_or(Value::Null),
            "plugins_json": w.get("plugins").cloned().unwrap_or(Value::Null),
            "webgl_parameters_json": w.get("webglParameters").cloned().unwrap_or(Value::Null),
            "webgl2_parameters_json": w.get("webgl2Parameters").cloned().unwrap_or(Value::Null),
            "http2": {
                "header_table_size": w.get("http2HeaderTableSize").cloned().unwrap_or(Value::Null),
                "initial_window_size": w.get("http2InitialWindowSize").cloned().unwrap_or(Value::Null),
                "max_concurrent_streams": w.get("http2MaxConcurrentStreams").cloned().unwrap_or(Value::Null),
                "max_frame_size": w.get("http2MaxFrameSize").cloned().unwrap_or(Value::Null),
                "max_header_list_size": w.get("http2MaxHeaderListSize").cloned().unwrap_or(Value::Null),
            },
            "tls_browser_type": w.get("tlsBrowserType").cloned().unwrap_or(Value::Null),
        },
    })
}

struct UaInfo {
    major: String,
    full: String,
    build: String,
    patch: String,
}

fn parse_ua(ua: &str) -> UaInfo {
    let default = UaInfo {
        major: "149".into(),
        full: "149.0.7827.116".into(),
        build: "7827".into(),
        patch: "116".into(),
    };
    let Some(idx) = ua.find("Chrome/") else {
        return default;
    };
    let rest = &ua[idx + 7..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let ver = &rest[..end];
    let parts: Vec<&str> = ver.split('.').collect();
    if parts.len() != 4 {
        return default;
    }
    UaInfo {
        major: parts[0].to_string(),
        full: ver.to_string(),
        build: parts[2].to_string(),
        patch: parts[3].to_string(),
    }
}

fn hex_to_u32(hex: &str) -> u32 {
    let mut cleaned = String::with_capacity(8);
    for c in hex.chars() {
        if c.is_ascii_hexdigit() {
            cleaned.push(c);
            if cleaned.len() == 8 {
                break;
            }
        }
    }
    if cleaned.is_empty() {
        return 1;
    }
    let n = u32::from_str_radix(&cleaned, 16).unwrap_or(0);
    if n == 0 {
        1
    } else {
        n
    }
}

fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == 0 {
        1
    } else {
        h
    }
}

fn webgl_block(w: &Value) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "vendor".into(),
        w.get("webglVendor").cloned().unwrap_or(json!("")),
    );
    out.insert(
        "renderer".into(),
        w.get("webglRenderer").cloned().unwrap_or(json!("")),
    );
    out.insert(
        "unmasked_vendor".into(),
        w.get("webglVendor").cloned().unwrap_or(json!("")),
    );
    out.insert(
        "unmasked_renderer".into(),
        w.get("webglRenderer").cloned().unwrap_or(json!("")),
    );
    out.insert("vendor_masked".into(), json!("WebKit"));
    out.insert("renderer_masked".into(), json!("WebKit WebGL"));

    merge_webgl_params(&mut out, w.get("webglParameters"));
    merge_webgl_params(&mut out, w.get("webgl2Parameters"));
    Value::Object(out)
}

fn merge_webgl_params(out: &mut serde_json::Map<String, Value>, raw: Option<&Value>) {
    let Some(s) = raw.and_then(|v| v.as_str()) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return;
    };
    let Some(obj) = v.as_object() else { return };
    if let Some(ext) = obj.get("extensions").and_then(|v| v.as_array()) {
        out.insert("extensions".into(), Value::Array(ext.clone()));
    }
    if let Some(n) = obj.get("3379") {
        out.insert("max_texture_size".into(), n.clone());
    }
    if let Some(n) = obj.get("34921") {
        out.insert("max_vertex_attribs".into(), n.clone());
    }
}

fn ch_platform(platform: &str, ua: &str) -> &'static str {
    if platform == "MacIntel" {
        return "macOS";
    }
    if platform.contains("Linux") || ua.contains("Linux") {
        return "Linux";
    }
    "Windows"
}

fn ch_architecture(ua: &str) -> &'static str {
    let lower = ua.to_ascii_lowercase();
    if lower.contains("arm64") || lower.contains("aarch64") {
        "arm"
    } else {
        "x86"
    }
}

fn ch_bitness(ua: &str) -> &'static str {
    if ua.contains("Win64") || ua.contains("x64") || ua.contains("x86_64") {
        "64"
    } else {
        "32"
    }
}

fn color_gamut(w: &Value) -> &'static str {
    if w.get("colorGamutRec2020")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "rec2020"
    } else if w
        .get("colorGamutP3")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "p3"
    } else if w
        .get("colorGamutSrgb")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "srgb"
    } else {
        ""
    }
}

fn accept_language(w: &Value) -> String {
    if let Some(langs) = w.get("languages").and_then(|v| v.as_array()) {
        if !langs.is_empty() {
            let mut parts = Vec::with_capacity(langs.len());
            for (i, l) in langs.iter().enumerate() {
                let s = l.as_str().unwrap_or("");
                if i == 0 {
                    parts.push(s.to_string());
                } else {
                    let q = 1.0 - (i as f64) * 0.1;
                    parts.push(format!("{s};q={:.1}", q));
                }
            }
            return parts.join(",");
        }
    }
    let l = w
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("en-US");
    format!("{l},en;q=0.9")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_to_u32_matches_prototype() {
        // Matches convert.mjs hexToU32: takes the first 8 hex chars only.
        // "deadbeef1234567890abcdef" → "deadbeef" → 0xDEADBEEF.
        assert_eq!(hex_to_u32("deadbeef1234"), 0xDEAD_BEEF);
        // First 8 chars are all zero → 0 → coerced to 1.
        assert_eq!(hex_to_u32("00000000abcdef"), 1);
        // Empty input → coerced to 1.
        assert_eq!(hex_to_u32(""), 1);
        assert_eq!(hex_to_u32("00000000"), 1);
        // Non-hex chars are stripped, leaving the leading 8 hex chars.
        assert_eq!(hex_to_u32("!!abcd1234!!"), 0xABCD_1234);
    }

    #[test]
    fn fnv1a_matches_prototype() {
        // Reference computed via convert.mjs fnv1a() (32-bit FNV-1a offset basis).
        assert_ne!(fnv1a("hello"), 0);
        // Empty input keeps the offset basis 0x811c9dc5 (non-zero, no coerce).
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        // Known reference: fnv1a("a") = 0xE40C292C (32-bit FNV-1a of a single byte 0x61).
        assert_eq!(fnv1a("a"), 0xE40C_292C);
        // Sanity: different inputs → different hashes.
        assert_ne!(fnv1a("foo"), fnv1a("bar"));
    }

    #[test]
    fn parse_ua_extracts_chrome_version() {
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/149.0.7827.116 Safari/537.36";
        let info = parse_ua(ua);
        assert_eq!(info.major, "149");
        assert_eq!(info.full, "149.0.7827.116");
        assert_eq!(info.build, "7827");
        assert_eq!(info.patch, "116");
    }

    #[test]
    fn conversion_shape_matches_shardx_schema() {
        let raw = serde_json::json!({
            "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/149.0.7827.116 Safari/537.36",
            "platform": "Win32",
            "canvasNoiseSeed": "deadbeef1234567890abcdef",
            "screenWidth": 1920, "screenHeight": 1080,
            "screenAvailWidth": 1920, "screenAvailHeight": 1040,
            "screenColorDepth": 24, "devicePixelRatio": 1.0,
            "hardwareConcurrency": 12, "deviceMemory": 8,
            "language": "en-US", "languages": ["en-US", "en"],
            "timezone": "America/New_York",
            "webglVendor": "Google Inc. (NVIDIA)",
            "webglRenderer": "ANGLE (NVIDIA, NVIDIA GeForce RTX 3080)",
            "webglParameters": "{\"3379\":16384,\"34921\":16,\"extensions\":[\"WEBGL_debug_renderer_info\"]}",
            "maxTouchPoints": 0,
        });
        let cfg = wayfern_to_shardx(&raw, Some("Test FP"));
        assert_eq!(cfg["name"], "Test FP");
        assert_eq!(cfg["navigator"]["user_agent"], raw["userAgent"]);
        assert_eq!(cfg["screen"]["width"], 1920);
        assert_eq!(cfg["client_hints"]["brand_version"], "149");
        assert_eq!(cfg["webgl"]["renderer"], raw["webglRenderer"]);
        assert_eq!(cfg["webgl"]["unmasked_renderer"], raw["webglRenderer"]);
        assert_eq!(cfg["webgl"]["max_texture_size"], 16384);
        assert_eq!(cfg["webgl"]["max_vertex_attribs"], 16);
        assert_eq!(cfg["webgl"]["extensions"][0], "WEBGL_debug_renderer_info");
        assert_eq!(cfg["noise"]["canvas"]["enabled"], false);
        assert!(cfg["noise"]["canvas"]["seed"].as_u64().unwrap() > 0);
        assert!(cfg["noise"]["webgl"]["seed"].as_u64().unwrap() > 0);
        assert_ne!(
            cfg["noise"]["canvas"]["seed"],
            cfg["noise"]["webgl"]["seed"]
        );
    }
}
