"""ShardX Python SDK — launch isolated anti-detect browser profiles from Python.

Quickstart:

    from shardx import ShardX

    sdk = ShardX()
    # Engine + Widevine + fingerprint library auto-download from CDN on
    # first call — no separate install step.

    # Launch a specific profile
    sess = sdk.launch("win-rtx4060", proxy="socks5://user:pass@host:port", cdp=True)
    print(sess.cdp_url)

    # Or pick a random profile (optionally filtered by platform)
    sess = sdk.launch(platform="Windows", randomize=True)

    sess.stop()

The SDK mirrors every pre-launch step the desktop launcher does:

  • host-aware hardware randomisation (cores bracketed against the
    real CPU, deviceMemory capped at the real RAM bucket)
  • screen strategy (cap to host on macOS, replace with host on Win/Linux)
  • resolve `"auto"` sentinels for timezone / navigator.language /
    geolocation through a live geo lookup over the bound proxy
  • SOCKS5 UDP_ASSOCIATE probe → QUIC + WebRTC policy
"""
from __future__ import annotations

import random
from typing import Optional, Union

from contextlib import asynccontextmanager

from patchright.async_api import Browser as PatchrightBrowser, async_playwright

from .auto_resolve import has_auto_fields, resolve_auto_fields
from .browser import Browser, BrowserSession
from .geo import GeoInfo, geo_check_via
from .host import (
    host_logical_cores,
    host_ram_bucket_gb,
    host_ram_gb,
    host_screen_size,
)
from .profile import FingerprintLibrary, Profile
from .proxy import ParsedProxy, parse_proxy, probe_udp
from .randomize import randomize_hardware, randomize_platform_version
from .runtime import RUNTIME_DIR, Runtime
from .screen import apply_screen_strategy, default_mode_for


class ShardX:
    """Top-level facade: bundles the runtime + fingerprint library + launcher."""

    def __init__(
        self,
        cache_dir: Optional[str] = None,
        profiles_dir: Optional[str] = None,
    ) -> None:
        """
        Args:
            cache_dir: where the engine, Widevine, and bundled fingerprint
                library live (defaults to the per-OS app-data dir).
            profiles_dir: per-profile user-data-dir root (cookies, IndexedDB,
                cache). Defaults to `./shardx-profiles/` relative to the
                running script — easy for users to find. Per-launch override
                also available via `launch(..., user_data_dir=...)`.
        """
        self.runtime = Runtime(cache_dir=cache_dir, profiles_dir=profiles_dir)
        self.library = FingerprintLibrary(self.runtime)
        self._browser = Browser(self.runtime)

    def list_profiles(self, *, platform: Optional[str] = None) -> list[str]:
        """Return bundled fingerprint ids, optionally filtered by platform.
        Auto-installs the fingerprint library on first call."""
        self.runtime.install()
        if platform:
            return list(self.library.filter(platform=platform))
        return self.library.ids()

    def random_profile(self, *, platform: Optional[str] = None) -> Profile:
        """Pick a random profile from the library (optionally platform-filtered).
        Auto-installs the fingerprint library on first call."""
        ids = self.list_profiles(platform=platform)
        if not ids:
            raise RuntimeError(
                f"No bundled profiles found{' for platform=' + platform if platform else ''}."
            )
        return self.library.load(random.choice(ids))

    def launch(
        self,
        fingerprint: Optional[Union[str, Profile, dict]] = None,
        *,
        platform: Optional[str] = None,
        randomize: bool = False,
        **kwargs,
    ) -> BrowserSession:
        """Launch a profile.

        Args:
            fingerprint: profile id (str), `Profile` instance, dict, or None
                to pick a random profile.
            platform: when `fingerprint` is None, filter the random pick by
                `navigator.platform` substring ("Windows" / "macOS" / "Linux").
            randomize: when True, freshly randomise `hardware_concurrency`,
                `device_memory` and `platform_version` before launch — mirrors
                what the desktop launcher does when you re-pick a GPU.
            proxy, cdp, headless, webrtc, webrtc_public_ip, quic, screen_mode,
            extra_args, env, probe_timeout: passed to `Browser.launch` — see
            its docstring.
        """
        self.runtime.install()
        if fingerprint is None:
            profile = self.random_profile(platform=platform)
        elif isinstance(fingerprint, str):
            profile = self.library.load(fingerprint)
        elif isinstance(fingerprint, Profile):
            profile = fingerprint
        elif isinstance(fingerprint, dict):
            profile = Profile(fingerprint)
        else:
            raise TypeError(
                f"fingerprint must be str, dict, Profile, or None; got {type(fingerprint).__name__}"
            )
        if randomize:
            randomize_hardware(profile.config, profile_id=profile.id)
            randomize_platform_version(profile.config)
        return self._browser.launch(profile, **kwargs)

    @asynccontextmanager
    async def session(self, fingerprint=None, **kwargs):
        """Async context manager: launches a profile AND attaches
        patchright, yielding a `patchright.async_api.Browser` ready to
        drive (no manual `connect_over_cdp` plumbing).

        Example:

            async with sdk.session("win-rtx4060", proxy="socks5://...") as browser:
                ctx = browser.contexts[0]
                page = await ctx.new_page()
                await page.goto("https://example.com")

        The underlying `BrowserSession` is attached as `browser._shardx`
        if you need `cdp_url`, `geo`, `proxy_udp_ms`, etc.
        """
        kwargs.setdefault("cdp", True)
        bsess = self.launch(fingerprint, **kwargs)
        if not bsess.cdp_url:
            bsess.stop()
            raise RuntimeError("CDP endpoint unavailable — engine failed to expose remote-debugging port")

        async with async_playwright() as pw:
            browser: PatchrightBrowser = await pw.chromium.connect_over_cdp(bsess.cdp_url)
            browser._shardx = bsess  # type: ignore[attr-defined]
            try:
                yield browser
            finally:
                try:
                    await browser.close()
                except Exception:
                    pass
                bsess.stop()

    def check_proxy(self, proxy_url: str) -> dict:
        """Validate a proxy URL before binding it to a profile.

        Returns a dict with the same fields the launcher uses to decide
        QUIC / WebRTC policy:

            {
              "udp_ms":              float | None,
              "geo":                 GeoInfo,
              "would_enable_quic":   bool,
              "would_set_webrtc":    "auto" | "tcp_only",
            }
        """
        parsed = parse_proxy(proxy_url)
        udp_ms = probe_udp(parsed) if parsed.is_socks5 else None
        geo = geo_check_via(parsed)
        udp_ok = udp_ms is not None
        return {
            "udp_ms": udp_ms,
            "geo": geo,
            "would_enable_quic": udp_ok,
            "would_set_webrtc": "auto" if udp_ok else "tcp_only",
        }


__all__ = [
    "ShardX",
    "Runtime", "RUNTIME_DIR",
    "Profile", "FingerprintLibrary",
    "Browser", "BrowserSession",
    "randomize_hardware", "randomize_platform_version",
    "ParsedProxy", "parse_proxy", "probe_udp",
    "host_logical_cores", "host_ram_gb", "host_ram_bucket_gb", "host_screen_size",
    "apply_screen_strategy", "default_mode_for",
    "GeoInfo", "geo_check_via",
    "has_auto_fields", "resolve_auto_fields",
]
__version__ = "0.1.0"
