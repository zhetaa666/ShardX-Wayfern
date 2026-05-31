"""Per-launch randomisation of hardware_concurrency / device_memory /
platform_version. Mirrors `randomize_hardware` and
`randomize_platform_version` in `src-tauri/src/lib.rs`.

Mac profiles use the curated MAC_HW_CONFIGS table (matched to the
profile id). Windows / Linux profiles use the launcher's host-bracketed
logic: real x86 logical-core counts filtered to the [host_cores-4,
host_cores+2] window, with device_memory floored by core count and
ceilinged by `host_ram_bucket_gb`.
"""
from __future__ import annotations

import random
from typing import Optional

from .host import host_logical_cores, host_ram_bucket_gb

MACOS_PLATFORM_VERSIONS = [
    "14.6.1", "14.7", "14.7.1", "14.7.2",
    "15.4", "15.4.1", "15.5", "15.6", "15.6.1", "15.7",
    "26.0", "26.0.1", "26.1",
]

# Win 10 ("10.0.0") + Win 11 21H2..25H2 ("13".."17"), weighted to common ones.
WINDOWS_PLATFORM_VERSIONS = [
    "10.0.0",
    "13.0.0",
    "14.0.0", "14.0.0", "14.0.0",
    "15.0.0", "15.0.0", "15.0.0", "15.0.0",
    "16.0.0", "16.0.0", "16.0.0",
    "17.0.0",
]

LINUX_PLATFORM_VERSIONS = [
    "5.15.0", "6.1.0", "6.5.0",
    "6.6.0", "6.8.0", "6.10.0", "6.11.0", "6.12.0",
    "6.14.0", "6.15.0", "6.16.0",
]

# (cores, mem_GiB) combos per Apple model id — keep in sync with lib.rs.
MAC_HW_CONFIGS: dict[str, list[tuple[int, int]]] = {
    "mac-m1-air13":     [(8, 8), (8, 16)],
    "mac-m1-mbp13":     [(8, 8), (8, 16)],
    "mac-m1-imac24":    [(8, 8), (8, 16)],
    "mac-m1-pro-mbp14": [(8, 16), (10, 16), (10, 32)],
    "mac-m1-pro-mbp16": [(8, 16), (10, 16), (10, 32)],
    "mac-m1-max-mbp14": [(10, 32)],
    "mac-m1-max-mbp16": [(10, 32)],
    "mac-m2-air13":     [(8, 8), (8, 16)],
    "mac-m2-air15":     [(8, 8), (8, 16)],
    "mac-m2-mbp13":     [(8, 8), (8, 16)],
    "mac-m2-pro-mbp14": [(10, 16), (12, 16), (12, 32)],
    "mac-m2-pro-mbp16": [(10, 16), (12, 16), (12, 32)],
    "mac-m2-max-mbp14": [(12, 32)],
    "mac-m2-max-mbp16": [(12, 32)],
    "mac-m3-air13":     [(8, 8), (8, 16)],
    "mac-m3-air15":     [(8, 8), (8, 16)],
    "mac-m3-mbp14":     [(8, 8), (8, 16)],
    "mac-m3-imac24":    [(8, 8), (8, 16)],
    "mac-m3-pro-mbp14": [(11, 16), (12, 16), (12, 32)],
    "mac-m3-pro-mbp16": [(11, 16), (12, 16), (12, 32)],
    "mac-m3-max-mbp14": [(14, 32), (16, 32)],
    "mac-m3-max-mbp16": [(14, 32), (16, 32)],
    "mac-m4-air13":     [(10, 16), (10, 32)],
    "mac-m4-air15":     [(10, 16), (10, 32)],
    "mac-m4-mbp14":     [(10, 16), (10, 32)],
    "mac-m4-imac24":    [(10, 16), (10, 32)],
    "mac-m4-pro-mbp14": [(12, 16), (14, 16), (14, 32)],
    "mac-m4-pro-mbp16": [(12, 16), (14, 16), (14, 32)],
    "mac-m4-max-mbp14": [(14, 32), (16, 32)],
    "mac-m4-max-mbp16": [(14, 32), (16, 32)],
    "mac-m5-mbp14":     [(10, 16), (10, 32)],
}

# Real x86 logical-core counts (SMT + Intel hybrid) — same array as lib.rs.
X86_CORES: list[int] = [4, 6, 8, 12, 16, 20, 24, 28, 32]


def randomize_platform_version(config: dict) -> None:
    """In-place: pick a fresh `navigator.platform_version` (+ mirror to client_hints)."""
    platform = ((config.get("navigator") or {}).get("platform") or "")
    pool = {
        "macOS":   MACOS_PLATFORM_VERSIONS,
        "Windows": WINDOWS_PLATFORM_VERSIONS,
        "Linux":   LINUX_PLATFORM_VERSIONS,
    }.get(platform)
    if not pool:
        return
    v = random.choice(pool)
    nav = config.setdefault("navigator", {})
    nav["platform_version"] = v
    ch = config.get("client_hints")
    if isinstance(ch, dict):
        ch["platform_version"] = v


def randomize_hardware(config: dict, profile_id: Optional[str] = None) -> None:
    """In-place: pick a fresh (hardware_concurrency, device_memory) pair.

    macOS: curated MAC_HW_CONFIGS table by profile id.
    Windows / Linux: bracket the host CPU count within [C-4, C+2] from the
    X86_CORES set; floor device_memory by core count (>=12 → 16, else 8) and
    cap by `host_ram_bucket_gb()`.
    """
    platform = ((config.get("navigator") or {}).get("platform") or "")

    if platform == "macOS" and profile_id and profile_id in MAC_HW_CONFIGS:
        cores, mem = random.choice(MAC_HW_CONFIGS[profile_id])
    elif platform in ("Windows", "Linux"):
        c = host_logical_cores()
        lo = max(0, c - 4)
        hi = c + 2
        candidates = [n for n in X86_CORES if lo <= n <= hi]
        if candidates:
            cores = random.choice(candidates)
        else:
            cores = min(X86_CORES, key=lambda n: abs(n - c))
        real = host_ram_bucket_gb()
        floor = 16 if cores >= 12 else 8
        mem_cand = [m for m in (8, 16, 32) if floor <= m <= real]
        mem = random.choice(mem_cand) if mem_cand else real
    else:
        return

    nav = config.setdefault("navigator", {})
    nav["hardware_concurrency"] = cores
    nav["device_memory"] = mem
