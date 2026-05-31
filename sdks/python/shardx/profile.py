"""Profile = a single fingerprint JSON + a working directory. Wraps the
bundled fingerprint library (~170 device profiles) and lets callers
override individual fields before launch."""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Iterable

from .runtime import Runtime


class Profile:
    """A fingerprint config + optional per-profile state. Mutable until launched."""

    def __init__(self, config: dict, *, id: str | None = None):
        self.config = dict(config)
        self.id = id or self.config.get("name") or "anonymous"

    @classmethod
    def from_file(cls, path: str | Path) -> "Profile":
        p = Path(path)
        cfg = json.loads(p.read_text())
        return cls(cfg, id=p.stem)

    def with_override(self, **overrides) -> "Profile":
        """Shallow merge overrides into navigator/screen/etc.

        Examples:
            p.with_override(name="my-account")
            p.with_override(navigator={"language": "en-US"})
        """
        out = json.loads(json.dumps(self.config))
        for k, v in overrides.items():
            if isinstance(v, dict) and isinstance(out.get(k), dict):
                out[k].update(v)
            else:
                out[k] = v
        return Profile(out, id=overrides.get("name", self.id))

    # ---- platform helpers ----

    @property
    def platform(self) -> str:
        return self.config.get("navigator", {}).get("platform", "")

    @property
    def has_webgpu(self) -> bool:
        wgp = self.config.get("webgpu")
        return wgp is not None and bool(wgp.get("limits"))


class FingerprintLibrary:
    """Wraps the JSON files under <runtime>/fingerprints/."""

    def __init__(self, runtime: Runtime):
        self._runtime = runtime

    def ids(self) -> list[str]:
        return sorted(p.stem for p in self._runtime.fingerprints_dir.glob("*.json"))

    def filter(self, *, platform: str | None = None) -> Iterable[str]:
        """Filter ids by `navigator.platform` substring match (case-insensitive)."""
        for fid in self.ids():
            try:
                p = self.load(fid)
            except Exception:
                continue
            if platform and platform.lower() not in p.platform.lower():
                continue
            yield fid

    def load(self, fingerprint_id: str) -> Profile:
        path = self._runtime.fingerprints_dir / f"{fingerprint_id}.json"
        if not path.exists():
            raise FileNotFoundError(
                f"Fingerprint '{fingerprint_id}' not found in {self._runtime.fingerprints_dir}. "
                f"Available: {', '.join(self.ids()[:10])}{'…' if len(self.ids()) > 10 else ''}"
            )
        return Profile.from_file(path)


def user_data_dir(runtime: Runtime, profile_id: str, base: Optional[Path] = None) -> Path:
    """Per-profile state (cookies, IndexedDB, cache) — preserved across launches.

    Defaults to `./shardx-profiles/<profile-id>/` (next to the running
    script). Override per-launch with `user_data_dir=...` or per-SDK
    with `ShardX(profiles_dir=...)`.
    """
    root = base if base is not None else runtime.profiles_root
    d = Path(root) / profile_id
    d.mkdir(parents=True, exist_ok=True)
    return d
