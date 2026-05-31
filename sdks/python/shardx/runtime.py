"""Runtime cache: download ShardX engine + Widevine CDM + fingerprint library
from the ProxyShard CDN, extract into a per-user cache dir, place Widevine
inside the engine bundle, and remember etags so subsequent runs are
zero-network. Mirrors src-tauri/src/runtime.rs in the launcher."""
from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

import httpx

PUB_BASE = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev"
CHROMIUM_VERSION = "148.0.7778.216"

# Default cache: ~/Library/Application Support/shardx-sdk (mac),
# %LOCALAPPDATA%\shardx-sdk (win), ~/.cache/shardx-sdk (linux).
def _default_cache_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "shardx-sdk"
    if sys.platform == "win32":
        return Path(os.environ.get("LOCALAPPDATA", Path.home())) / "shardx-sdk"
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "shardx-sdk"

RUNTIME_DIR = _default_cache_dir()


@dataclass(frozen=True)
class Archive:
    key: str           # filename in R2 bucket
    label: str         # human-readable for progress callbacks


@dataclass(frozen=True)
class HostSpec:
    browser: Archive
    widevine: Optional[Archive]
    binary_subpath: tuple[str, ...]   # path under runtime/ to the executable
    widevine_subpath: tuple[str, ...] # destination for the WidevineCdm dir


def host_spec() -> HostSpec:
    sysname = sys.platform
    arch = platform.machine().lower()
    if sysname == "darwin" and arch in ("arm64", "aarch64"):
        return HostSpec(
            browser=Archive("ShardX-Mac-arm64.zip", "ShardX browser (macOS arm64)"),
            widevine=Archive("ShardX-Widevine-Mac-arm64.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Mac-arm64", "ShardX.app", "Contents", "MacOS", "ShardX"),
            widevine_subpath=("ShardX-Mac-arm64", "ShardX.app", "Contents", "Frameworks",
                              "ShardX Framework.framework", "Versions", CHROMIUM_VERSION,
                              "Libraries", "WidevineCdm"),
        )
    if sysname == "win32" and arch in ("amd64", "x86_64"):
        return HostSpec(
            browser=Archive("ShardX-Windows.zip", "ShardX browser (Windows x64)"),
            widevine=Archive("ShardX-Widevine-Win.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Windows", "chrome.exe"),
            widevine_subpath=("ShardX-Windows", "WidevineCdm"),
        )
    if sysname.startswith("linux") and arch in ("x86_64", "amd64"):
        return HostSpec(
            browser=Archive("ShardX-Linux.zip", "ShardX browser (Linux x64)"),
            widevine=Archive("ShardX-Widevine-Linux.zip", "Widevine CDM"),
            binary_subpath=("ShardX-Linux", "chrome"),
            widevine_subpath=("ShardX-Linux", "WidevineCdm"),
        )
    raise RuntimeError(
        f"Unsupported host: {sysname}/{arch}. ShardX ships mac-arm64, win-x64, linux-x64."
    )


FINGERPRINTS_ARCHIVE = Archive("ShardX-Fingerprints.zip", "Fingerprint library")
FINGERPRINTS_TOP_DIR = "shardx-fingerprints"


ProgressCb = Callable[[str, int, int], None]   # (label, received, total)


class Runtime:
    """Owns the cache dir and the install/update lifecycle."""

    def __init__(
        self,
        cache_dir: Optional[str | Path] = None,
        progress: Optional[ProgressCb] = None,
        profiles_dir: Optional[str | Path] = None,
    ):
        self.root = Path(cache_dir) if cache_dir else RUNTIME_DIR
        self.root.mkdir(parents=True, exist_ok=True)
        # Per-profile user-data-dir tree.  Defaults to `./shardx-profiles/`
        # next to the running script so the user can find cookies / cache
        # easily; override with `profiles_dir=...`.  Engine assets stay
        # in `cache_dir`.
        self._profiles_root = Path(profiles_dir).resolve() if profiles_dir else None
        self._progress = progress
        self._spec = host_spec()
        # Set to True after a successful in-process install() so subsequent
        # launches in the same process skip the R2 HEAD round-trip (~1 s
        # over a clean connection).  Cleared by `install(force=True)`.
        self._checked_in_process = False

    @property
    def profiles_root(self) -> Path:
        d = self._profiles_root if self._profiles_root else self.root / "profiles"
        d.mkdir(parents=True, exist_ok=True)
        return d

    # ---- paths ----

    @property
    def manifest_path(self) -> Path:
        return self.root / "manifest.json"

    @property
    def binary_path(self) -> Path:
        return self.root.joinpath(*self._spec.binary_subpath)

    @property
    def fingerprints_dir(self) -> Path:
        d = self.root / "fingerprints"
        d.mkdir(parents=True, exist_ok=True)
        return d

    @property
    def installed(self) -> bool:
        return self.binary_path.exists()

    # ---- manifest ----

    def _load_manifest(self) -> dict:
        try:
            return json.loads(self.manifest_path.read_text())
        except Exception:
            return {}

    def _save_manifest(self, m: dict) -> None:
        self.manifest_path.write_text(json.dumps(m, indent=2))

    # ---- install ----

    def install(self, force: bool = False) -> None:
        """Idempotent — re-checks remote etag, skips when nothing changed.
        Within a single process, subsequent calls are no-ops unless `force=True`.
        """
        if self._checked_in_process and not force:
            return
        local = self._load_manifest()
        # Browser
        need_browser = force or not self.installed or \
            local.get("browser_etag") != self._head_etag(self._spec.browser.key)
        if need_browser:
            etag = self._download_and_extract(self._spec.browser, self.root)
            local["browser_etag"] = etag
        # Widevine — only re-pull when browser changed (versions must match).
        if self._spec.widevine and (need_browser or not local.get("widevine_etag")):
            etag = self._download_and_extract(self._spec.widevine, self.root)
            self._place_widevine()
            local["widevine_etag"] = etag
        # Fingerprints — additive seed (etag changed → re-extract, never
        # overwrites user-renamed files).
        fp_remote = self._head_etag(FINGERPRINTS_ARCHIVE.key)
        if force or local.get("fingerprints_etag") != fp_remote or not any(self.fingerprints_dir.glob("*.json")):
            self._install_fingerprints(force=force)
            local["fingerprints_etag"] = fp_remote
        self._save_manifest(local)
        # Make binary executable on unix.
        if sys.platform != "win32" and self.binary_path.exists():
            self.binary_path.chmod(self.binary_path.stat().st_mode | 0o111)
        self._checked_in_process = True

    def _head_etag(self, key: str) -> Optional[str]:
        try:
            with httpx.Client(timeout=8.0) as c:
                r = c.head(f"{PUB_BASE}/{key}")
                if r.status_code != 200:
                    return None
                return r.headers.get("etag", "").strip('"') or None
        except Exception:
            return None

    def _download_and_extract(self, arch: Archive, dest: Path) -> str:
        url = f"{PUB_BASE}/{arch.key}"
        tmp = dest / f".{arch.key}.tmp"
        tmp.parent.mkdir(parents=True, exist_ok=True)
        etag = ""
        with httpx.stream("GET", url, timeout=None, follow_redirects=True) as r:
            r.raise_for_status()
            etag = r.headers.get("etag", "").strip('"')
            total = int(r.headers.get("content-length", 0))
            received = 0
            with tmp.open("wb") as f:
                for chunk in r.iter_bytes(chunk_size=1 << 16):
                    f.write(chunk)
                    received += len(chunk)
                    if self._progress:
                        self._progress(arch.label, received, total)
        # Extract.  IMPORTANT: on macOS/Linux we shell out to the system
        # `unzip` instead of Python's `zipfile` because zipfile cannot
        # restore symlinks (every `Versions/Current/...` link in a `.app`
        # framework gets written as a 24-byte text file) and drops the
        # +x permission bits on every helper executable.  The result
        # extracts cleanly but fails to launch — GPU helper can't find
        # the framework dylib and the engine FATALs on first child.
        if sys.platform == "win32":
            with zipfile.ZipFile(tmp) as z:
                z.extractall(dest)
        else:
            _system_unzip(tmp, dest)
        tmp.unlink(missing_ok=True)
        return etag

    def _place_widevine(self) -> None:
        if not self._spec.widevine:
            return
        # Source dir inside the extracted Widevine archive (mirrors the
        # `ShardX-Widevine-<plat>/WidevineCdm` layout from the launcher).
        wrapper_name = self._spec.widevine.key.removesuffix(".zip")
        src = self.root / wrapper_name / "WidevineCdm"
        if not src.exists():
            return
        dst = self.root.joinpath(*self._spec.widevine_subpath)
        if dst.exists():
            shutil.rmtree(dst)
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dst))
        shutil.rmtree(self.root / wrapper_name, ignore_errors=True)

    def _install_fingerprints(self, force: bool) -> None:
        url = f"{PUB_BASE}/{FINGERPRINTS_ARCHIVE.key}"
        staging = self.fingerprints_dir / ".staging"
        if staging.exists():
            shutil.rmtree(staging)
        staging.mkdir(parents=True, exist_ok=True)
        tmp = staging / "bundle.zip"
        with httpx.stream("GET", url, timeout=None, follow_redirects=True) as r:
            r.raise_for_status()
            total = int(r.headers.get("content-length", 0))
            received = 0
            with tmp.open("wb") as f:
                for chunk in r.iter_bytes(chunk_size=1 << 16):
                    f.write(chunk)
                    received += len(chunk)
                    if self._progress:
                        self._progress(FINGERPRINTS_ARCHIVE.label, received, total)
        # Fingerprints bundle is plain JSON files — `zipfile` is fine
        # everywhere (no symlinks / exec bits to preserve).
        with zipfile.ZipFile(tmp) as z:
            z.extractall(staging)
        # Move *.json from the wrapper dir into fingerprints/, additive
        # (never clobber user-edited files unless force).
        src_dir = staging / FINGERPRINTS_TOP_DIR
        walk = src_dir if src_dir.exists() else staging
        for p in walk.iterdir():
            if p.suffix == ".json":
                dst = self.fingerprints_dir / p.name
                if force or not dst.exists():
                    shutil.copy(p, dst)
        shutil.rmtree(staging, ignore_errors=True)


def _system_unzip(archive: Path, dest: Path) -> None:
    """Extract via /usr/bin/unzip — preserves symlinks and permission
    bits that Python's zipfile silently drops.  Required for any
    macOS .app bundle (Versions/Current symlinks + Helper exec bits).
    """
    dest.mkdir(parents=True, exist_ok=True)
    try:
        subprocess.run(
            ["unzip", "-q", "-o", str(archive), "-d", str(dest)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    except FileNotFoundError as e:
        raise RuntimeError(
            "system `unzip` not found — required for symlink-preserving "
            "extraction on macOS / Linux"
        ) from e
    except subprocess.CalledProcessError as e:
        raise RuntimeError(
            f"unzip failed for {archive.name}: {e.stderr.decode(errors='replace')[:400]}"
        ) from e
