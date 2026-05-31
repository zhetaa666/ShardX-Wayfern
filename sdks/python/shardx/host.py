"""Host machine introspection — logical CPU count, physical RAM and the
primary monitor resolution. Mirrors `host_logical_cores`, `host_ram_gb`,
`host_ram_bucket_gb` in `src-tauri/src/lib.rs` and the screen probing
the launcher does via `tauri::WebviewWindow::primary_monitor`.

All probes are best-effort: an `OSError` / parse failure returns `None`
(or the documented fallback) instead of raising.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from typing import Optional, Tuple


def host_logical_cores() -> int:
    """Logical CPU count (SMT threads). Falls back to 8 if `os.cpu_count`
    returns `None` — same fallback the launcher uses."""
    n = os.cpu_count()
    return int(n) if n and n > 0 else 8


def host_ram_gb() -> Optional[int]:
    """Physical RAM in GiB, best-effort per OS. None on failure."""
    try:
        if sys.platform == "darwin":
            out = subprocess.check_output(
                ["sysctl", "-n", "hw.memsize"],
                stderr=subprocess.DEVNULL, timeout=2.0,
            ).decode().strip()
            return int(int(out) // (1024 ** 3))
        if sys.platform.startswith("linux"):
            with open("/proc/meminfo", "r") as f:
                for line in f:
                    if line.startswith("MemTotal:"):
                        kb = int(line.split()[1])
                        return int(kb // (1024 * 1024))
            return None
        if sys.platform == "win32":
            # Prefer PowerShell + CIM — wmic is deprecated on recent Windows.
            ps = shutil.which("powershell") or shutil.which("pwsh")
            if not ps:
                return None
            out = subprocess.check_output(
                [ps, "-NoProfile", "-Command",
                 "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"],
                stderr=subprocess.DEVNULL, timeout=4.0,
            ).decode().strip()
            return int(int(out) // (1024 ** 3))
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    return None


def host_ram_bucket_gb() -> int:
    """Round host RAM to Chrome's `deviceMemory` bucket {8, 16, 32};
    None → 16 (launcher default)."""
    gb = host_ram_gb()
    if gb is None:
        return 16
    if gb >= 32:
        return 32
    if gb >= 16:
        return 16
    return 8


# ---- Primary monitor (width, height) ----

_MAC_RES_RE = re.compile(r"Resolution:\s*(\d+)\s*x\s*(\d+)")
_XRANDR_CUR_RE = re.compile(r"^\s*(\d+)x(\d+)[^\n]*\*", re.MULTILINE)


def host_screen_size() -> Optional[Tuple[int, int]]:
    """Primary monitor (width, height) in CSS pixels, or None on failure."""
    try:
        if sys.platform == "darwin":
            return _mac_screen_size()
        if sys.platform == "win32":
            return _windows_screen_size()
        if sys.platform.startswith("linux"):
            return _linux_screen_size()
    except Exception:
        return None
    return None


def _mac_screen_size() -> Optional[Tuple[int, int]]:
    # `system_profiler SPDisplaysDataType` reports panel resolution
    # (not the post-Retina desktop bounds), which is what the launcher's
    # primary_monitor() returns once it divides by scale_factor.
    try:
        out = subprocess.check_output(
            ["system_profiler", "SPDisplaysDataType"],
            stderr=subprocess.DEVNULL, timeout=4.0,
        ).decode()
    except (OSError, subprocess.SubprocessError):
        return None
    best: Optional[Tuple[int, int]] = None
    for m in _MAC_RES_RE.finditer(out):
        w, h = int(m.group(1)), int(m.group(2))
        if best is None or (w * h) > (best[0] * best[1]):
            best = (w, h)
    return best


def _windows_screen_size() -> Optional[Tuple[int, int]]:
    try:
        import ctypes
        user32 = ctypes.windll.user32   # type: ignore[attr-defined]
        # Make per-monitor DPI aware so GetSystemMetrics returns physical px;
        # silently ignored on older Windows.
        try:
            ctypes.windll.shcore.SetProcessDpiAwareness(2)   # type: ignore[attr-defined]
        except Exception:
            try:
                user32.SetProcessDPIAware()
            except Exception:
                pass
        w = int(user32.GetSystemMetrics(0))  # SM_CXSCREEN
        h = int(user32.GetSystemMetrics(1))  # SM_CYSCREEN
        if w > 0 and h > 0:
            return (w, h)
    except Exception:
        return None
    return None


def _linux_screen_size() -> Optional[Tuple[int, int]]:
    try:
        out = subprocess.check_output(
            ["xrandr", "--query"],
            stderr=subprocess.DEVNULL, timeout=4.0,
        ).decode()
    except (OSError, subprocess.SubprocessError):
        return None
    m = _XRANDR_CUR_RE.search(out)
    if m:
        return (int(m.group(1)), int(m.group(2)))
    return None
