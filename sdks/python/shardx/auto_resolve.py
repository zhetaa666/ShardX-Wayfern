"""Resolve `"auto"` sentinels in a profile config — port of
`resolve_auto_fields` in `src-tauri/src/launch.rs`. Reads the live geo
(through the bound proxy when present, direct otherwise), falls back to
the host TZ/locale on failure, then mutates `cfg` in place to write
concrete timezone / navigator.language / accept_language / languages /
icu_locale / geolocation values."""
from __future__ import annotations

import os
import sys
import time
from typing import Optional, Tuple

from .geo import GeoInfo, geo_check_via
from .proxy import ParsedProxy


def _country_to_locale(cc: str) -> str:
    """ISO-3166 alpha-2 → BCP-47 locale. Ported 1:1 from the launcher's
    Rust `country_to_locale` (src-tauri/src/proxy.rs).  Authoritative
    table the desktop launcher uses — keep in sync if the Rust side
    ever changes."""
    return _CC_TO_LOCALE.get((cc or "").upper(), "en-US")


def _country_to_timezone(cc: str) -> str:
    """Country → IANA timezone fallback for providers that omit timezone.
    Ported 1:1 from launcher's Rust `country_to_timezone`."""
    return _CC_TO_TZ.get((cc or "").upper(), "UTC")


_CC_TO_TZ: dict[str, str] = {
    "US": "America/New_York", "CA": "America/Toronto",
    "GB": "Europe/London", "UK": "Europe/London",
    "DE": "Europe/Berlin", "FR": "Europe/Paris", "ES": "Europe/Madrid",
    "IT": "Europe/Rome", "NL": "Europe/Amsterdam", "PL": "Europe/Warsaw",
    "PT": "Europe/Lisbon", "RO": "Europe/Bucharest", "RU": "Europe/Moscow",
    "UA": "Europe/Kyiv", "TR": "Europe/Istanbul", "GR": "Europe/Athens",
    "CZ": "Europe/Prague", "HU": "Europe/Budapest",
    "SE": "Europe/Stockholm", "FI": "Europe/Helsinki",
    "NO": "Europe/Oslo", "DK": "Europe/Copenhagen",
    "CH": "Europe/Zurich", "AT": "Europe/Vienna",
    "BR": "America/Sao_Paulo", "AR": "America/Argentina/Buenos_Aires",
    "MX": "America/Mexico_City",
    "AU": "Australia/Sydney", "NZ": "Pacific/Auckland",
    "IN": "Asia/Kolkata", "ID": "Asia/Jakarta", "MY": "Asia/Kuala_Lumpur",
    "SG": "Asia/Singapore", "TH": "Asia/Bangkok", "VN": "Asia/Ho_Chi_Minh",
    "CN": "Asia/Shanghai", "HK": "Asia/Hong_Kong", "TW": "Asia/Taipei",
    "JP": "Asia/Tokyo", "KR": "Asia/Seoul",
    "IL": "Asia/Jerusalem", "SA": "Asia/Riyadh", "AE": "Asia/Dubai",
}


_CC_TO_LOCALE: dict[str, str] = {
    "US": "en-US", "GB": "en-GB", "UK": "en-GB", "CA": "en-CA",
    "AU": "en-AU", "NZ": "en-NZ", "IE": "en-IE", "ZA": "en-ZA", "IN": "en-IN",
    "DE": "de-DE", "AT": "de-AT", "CH": "de-CH",
    "FR": "fr-FR", "BE": "fr-BE",
    "ES": "es-ES", "MX": "es-MX", "AR": "es-AR", "CO": "es-CO", "CL": "es-CL",
    "IT": "it-IT", "NL": "nl-NL", "PL": "pl-PL",
    "BR": "pt-BR", "PT": "pt-PT",
    "RO": "ro-RO", "RU": "ru-RU", "BY": "be-BY", "UA": "uk-UA",
    "TR": "tr-TR", "GR": "el-GR",
    "CZ": "cs-CZ", "SK": "sk-SK", "HU": "hu-HU",
    "SE": "sv-SE", "FI": "fi-FI", "NO": "nb-NO", "DK": "da-DK",
    "BG": "bg-BG", "HR": "hr-HR", "SI": "sl-SI", "RS": "sr-RS",
    "IL": "he-IL",
    "SA": "ar-SA", "AE": "ar-SA", "EG": "ar-SA",
    "ID": "id-ID", "MY": "ms-MY", "PH": "fil-PH", "VN": "vi-VN", "TH": "th-TH",
    "CN": "zh-CN", "HK": "zh-HK", "TW": "zh-TW",
    "JP": "ja-JP", "KR": "ko-KR",
}


def has_auto_fields(cfg: dict) -> bool:
    """True when at least one auto-resolvable sentinel is present."""
    if cfg.get("timezone") == "auto":
        return True
    nav = cfg.get("navigator") or {}
    if nav.get("language") == "auto":
        return True
    geo = cfg.get("geolocation") or {}
    if isinstance(geo, dict) and geo.get("mode") == "auto":
        return True
    return False


def _host_timezone() -> Optional[str]:
    # `time.tzname` is a tuple like ("EST", "EDT"). The launcher prefers an
    # IANA name; we use $TZ if it looks like one.
    tz = os.environ.get("TZ", "").strip()
    if tz and "/" in tz:
        return tz
    # /etc/localtime symlink on Unix.
    try:
        target = os.readlink("/etc/localtime")
        for prefix in ("/usr/share/zoneinfo/", "/var/db/timezone/zoneinfo/"):
            i = target.find(prefix)
            if i >= 0:
                return target[i + len(prefix):]
    except OSError:
        pass
    # Fallback to short name from time.tzname (not IANA, but the launcher
    # also surfaces non-IANA values here).
    try:
        names = time.tzname
        if names and names[0]:
            return names[0]
    except Exception:
        pass
    return None


def _host_locale() -> str:
    for var in ("LANG", "LC_ALL", "LC_MESSAGES"):
        v = os.environ.get(var, "")
        if not v:
            continue
        stripped = v.split(".", 1)[0].replace("_", "-")
        if "-" in stripped:
            return stripped
    return "en-US"


def resolve_auto_fields(cfg: dict, proxy: Optional[ParsedProxy]) -> Optional[GeoInfo]:
    """Apply the launcher's "auto" resolution. Returns the GeoInfo that
    actually fed the resolution (or None when both proxy + direct probes
    failed and the host fallback was used)."""
    want_tz   = cfg.get("timezone") == "auto"
    want_lang = (cfg.get("navigator") or {}).get("language") == "auto"
    geo_dict  = cfg.get("geolocation") or {}
    want_geo  = isinstance(geo_dict, dict) and geo_dict.get("mode") == "auto"
    if not (want_tz or want_lang or want_geo):
        return None

    geo: Optional[GeoInfo] = None
    if proxy is not None:
        try:
            geo = geo_check_via(proxy)
        except Exception:
            geo = None
    if geo is None:
        # Direct fallback (also the path for no-proxy launches).
        try:
            geo = geo_check_via(None)
        except Exception:
            geo = None

    if geo is not None:
        # Timezone: always from API. If the provider didn't return one
        # (some IPs / providers lack it), fall back to the country-code
        # table — NOT the host TZ (would leak the launcher's real zone).
        resolved_tz = geo.timezone or _country_to_timezone(geo.country_code)
        resolved_locale = _country_to_locale(geo.country_code)
        lat = geo.latitude if geo.latitude != 0.0 else None
        lng = geo.longitude if geo.longitude != 0.0 else None
    else:
        resolved_tz = _host_timezone() or "UTC"
        resolved_locale = _host_locale()
        lat = lng = None

    if want_tz:
        cfg["timezone"] = resolved_tz

    if want_lang:
        base = resolved_locale.split("-", 1)[0]
        if resolved_locale == "en-US":
            accept = "en-US,en;q=0.9"
            languages = ["en-US", "en"]
        else:
            accept = f"{resolved_locale},{base};q=0.9,en-US;q=0.8,en;q=0.7"
            languages = [resolved_locale, base, "en-US", "en"]
        nav = cfg.setdefault("navigator", {})
        nav["language"] = resolved_locale
        nav["accept_language"] = accept
        nav["languages"] = languages
        # Always overwrite — matches launch.rs (even hardcoded values are replaced).
        cfg["icu_locale"] = resolved_locale

    if want_geo:
        if lat is not None and lng is not None:
            cfg["geolocation"] = {
                "mode": "manual",
                "latitude": lat,
                "longitude": lng,
                "accuracy": 50.0,
            }
        else:
            cfg.pop("geolocation", None)

    return geo
