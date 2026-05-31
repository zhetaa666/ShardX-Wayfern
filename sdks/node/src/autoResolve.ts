// Resolve `"auto"` sentinels in a profile config — port of
// `resolve_auto_fields` in `src-tauri/src/launch.rs`. Reads the live geo
// (through the bound proxy when present, direct otherwise), falls back to
// the host TZ/locale on failure, then mutates `cfg` in place to write
// concrete timezone / navigator.language / accept_language / languages /
// icu_locale / geolocation values.
import { readlinkSync } from "node:fs";

import { geoCheckVia, type GeoInfo } from "./geo.js";
import type { ParsedProxy } from "./proxy.js";


/** ISO-3166 alpha-2 → BCP-47 locale.  Ported 1:1 from launcher's Rust
 *  `country_to_locale` (src-tauri/src/proxy.rs).  Authoritative table the
 *  desktop launcher uses — keep in sync if the Rust side ever changes. */
function countryToLocale(cc: string): string {
  return CC_TO_LOCALE[(cc ?? "").toUpperCase()] ?? "en-US";
}

const CC_TO_LOCALE: Record<string, string> = {
  US: "en-US", GB: "en-GB", UK: "en-GB", CA: "en-CA",
  AU: "en-AU", NZ: "en-NZ", IE: "en-IE", ZA: "en-ZA", IN: "en-IN",
  DE: "de-DE", AT: "de-AT", CH: "de-CH",
  FR: "fr-FR", BE: "fr-BE",
  ES: "es-ES", MX: "es-MX", AR: "es-AR", CO: "es-CO", CL: "es-CL",
  IT: "it-IT", NL: "nl-NL", PL: "pl-PL",
  BR: "pt-BR", PT: "pt-PT",
  RO: "ro-RO", RU: "ru-RU", BY: "be-BY", UA: "uk-UA",
  TR: "tr-TR", GR: "el-GR",
  CZ: "cs-CZ", SK: "sk-SK", HU: "hu-HU",
  SE: "sv-SE", FI: "fi-FI", NO: "nb-NO", DK: "da-DK",
  BG: "bg-BG", HR: "hr-HR", SI: "sl-SI", RS: "sr-RS",
  IL: "he-IL",
  SA: "ar-SA", AE: "ar-SA", EG: "ar-SA",
  ID: "id-ID", MY: "ms-MY", PH: "fil-PH", VN: "vi-VN", TH: "th-TH",
  CN: "zh-CN", HK: "zh-HK", TW: "zh-TW",
  JP: "ja-JP", KR: "ko-KR",
};

/** Country → IANA timezone fallback for providers that omit timezone.
 *  Ported 1:1 from launcher's Rust `country_to_timezone`. */
function countryToTimezone(cc: string): string {
  return CC_TO_TZ[(cc ?? "").toUpperCase()] ?? "UTC";
}

const CC_TO_TZ: Record<string, string> = {
  US: "America/New_York", CA: "America/Toronto",
  GB: "Europe/London", UK: "Europe/London",
  DE: "Europe/Berlin", FR: "Europe/Paris", ES: "Europe/Madrid",
  IT: "Europe/Rome", NL: "Europe/Amsterdam", PL: "Europe/Warsaw",
  PT: "Europe/Lisbon", RO: "Europe/Bucharest", RU: "Europe/Moscow",
  UA: "Europe/Kyiv", TR: "Europe/Istanbul", GR: "Europe/Athens",
  CZ: "Europe/Prague", HU: "Europe/Budapest",
  SE: "Europe/Stockholm", FI: "Europe/Helsinki",
  NO: "Europe/Oslo", DK: "Europe/Copenhagen",
  CH: "Europe/Zurich", AT: "Europe/Vienna",
  BR: "America/Sao_Paulo", AR: "America/Argentina/Buenos_Aires",
  MX: "America/Mexico_City",
  AU: "Australia/Sydney", NZ: "Pacific/Auckland",
  IN: "Asia/Kolkata", ID: "Asia/Jakarta", MY: "Asia/Kuala_Lumpur",
  SG: "Asia/Singapore", TH: "Asia/Bangkok", VN: "Asia/Ho_Chi_Minh",
  CN: "Asia/Shanghai", HK: "Asia/Hong_Kong", TW: "Asia/Taipei",
  JP: "Asia/Tokyo", KR: "Asia/Seoul",
  IL: "Asia/Jerusalem", SA: "Asia/Riyadh", AE: "Asia/Dubai",
};

export function hasAutoFields(cfg: Record<string, unknown>): boolean {
  if (cfg["timezone"] === "auto") return true;
  const nav = cfg["navigator"] as Record<string, unknown> | undefined;
  if (nav && nav["language"] === "auto") return true;
  const geo = cfg["geolocation"] as Record<string, unknown> | undefined;
  if (geo && typeof geo === "object" && geo["mode"] === "auto") return true;
  return false;
}

function hostTimezone(): string | null {
  const tz = (process.env.TZ ?? "").trim();
  if (tz && tz.includes("/")) return tz;
  try {
    const target = readlinkSync("/etc/localtime");
    for (const prefix of ["/usr/share/zoneinfo/", "/var/db/timezone/zoneinfo/"]) {
      const i = target.indexOf(prefix);
      if (i >= 0) return target.slice(i + prefix.length);
    }
  } catch { /* not a symlink / not unix */ }
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || null;
  } catch {
    return null;
  }
}

function hostLocale(): string {
  for (const v of [process.env.LANG, process.env.LC_ALL, process.env.LC_MESSAGES]) {
    if (!v) continue;
    const stripped = v.split(".", 1)[0].replace(/_/g, "-");
    if (stripped.includes("-")) return stripped;
  }
  return "en-US";
}

/**
 * Apply the launcher's "auto" resolution. Returns the GeoInfo that fed the
 * resolution, or null when both proxy + direct probes failed and the host
 * fallback was used.
 */
export async function resolveAutoFields(
  cfg: Record<string, unknown>,
  proxy: ParsedProxy | null,
): Promise<GeoInfo | null> {
  const wantTz = cfg["timezone"] === "auto";
  const nav = cfg["navigator"] as Record<string, unknown> | undefined;
  const wantLang = !!(nav && nav["language"] === "auto");
  const geoCfg = cfg["geolocation"] as Record<string, unknown> | undefined;
  const wantGeo = !!(geoCfg && typeof geoCfg === "object" && geoCfg["mode"] === "auto");
  if (!wantTz && !wantLang && !wantGeo) return null;

  let geo: GeoInfo | null = null;
  if (proxy) {
    try { geo = await geoCheckVia(proxy); } catch { geo = null; }
  }
  if (!geo) {
    try { geo = await geoCheckVia(null); } catch { geo = null; }
  }

  let resolvedTz: string;
  let resolvedLocale: string;
  let lat: number | null;
  let lng: number | null;
  if (geo) {
    // Timezone: always from API. If the provider didn't return one,
    // fall back to the country-code table — NOT the host TZ (would
    // leak the launcher's real zone).
    resolvedTz = geo.timezone || countryToTimezone(geo.countryCode);
    resolvedLocale = countryToLocale(geo.countryCode);
    lat = geo.latitude !== 0 ? geo.latitude : null;
    lng = geo.longitude !== 0 ? geo.longitude : null;
  } else {
    resolvedTz = hostTimezone() ?? "UTC";
    resolvedLocale = hostLocale();
    lat = null;
    lng = null;
  }

  if (wantTz) cfg["timezone"] = resolvedTz;

  if (wantLang) {
    const base = resolvedLocale.split("-", 1)[0];
    const accept = resolvedLocale === "en-US"
      ? "en-US,en;q=0.9"
      : `${resolvedLocale},${base};q=0.9,en-US;q=0.8,en;q=0.7`;
    const languages = resolvedLocale === "en-US"
      ? ["en-US", "en"]
      : [resolvedLocale, base, "en-US", "en"];
    const navObj = (cfg["navigator"] ??= {}) as Record<string, unknown>;
    navObj["language"] = resolvedLocale;
    navObj["accept_language"] = accept;
    navObj["languages"] = languages;
    // Always overwrite — matches launch.rs (even hardcoded values are replaced).
    cfg["icu_locale"] = resolvedLocale;
  }

  if (wantGeo) {
    if (lat !== null && lng !== null) {
      cfg["geolocation"] = {
        mode: "manual",
        latitude: lat,
        longitude: lng,
        accuracy: 50.0,
      };
    } else {
      delete cfg["geolocation"];
    }
  }

  return geo;
}
