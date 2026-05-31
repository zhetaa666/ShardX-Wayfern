// Host machine introspection — logical CPU count, physical RAM and the
// primary monitor resolution. Mirrors `host_logical_cores`, `host_ram_gb`,
// `host_ram_bucket_gb` in `src-tauri/src/lib.rs`.
//
// All probes are best-effort: a thrown error returns `null` (or the
// documented fallback) instead of propagating.
import { cpus, platform as osPlatform } from "node:os";
import { execFileSync, execSync } from "node:child_process";
import { readFileSync } from "node:fs";

/** Logical CPU count (SMT threads). Falls back to 8 if `os.cpus()` is empty. */
export function hostLogicalCores(): number {
  try {
    const n = cpus().length;
    return n > 0 ? n : 8;
  } catch {
    return 8;
  }
}

/** Physical RAM in GiB, best-effort per OS. `null` on failure. */
export function hostRamGb(): number | null {
  const plat = osPlatform();
  try {
    if (plat === "darwin") {
      const out = execFileSync("sysctl", ["-n", "hw.memsize"],
        { stdio: ["ignore", "pipe", "ignore"], timeout: 2000 }).toString().trim();
      const bytes = Number(out);
      if (!Number.isFinite(bytes) || bytes <= 0) return null;
      return Math.floor(bytes / (1024 ** 3));
    }
    if (plat === "linux") {
      const txt = readFileSync("/proc/meminfo", "utf8");
      for (const line of txt.split("\n")) {
        if (line.startsWith("MemTotal:")) {
          const kb = Number(line.split(/\s+/)[1]);
          if (!Number.isFinite(kb) || kb <= 0) return null;
          return Math.floor(kb / (1024 * 1024));
        }
      }
      return null;
    }
    if (plat === "win32") {
      const out = execSync(
        'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"',
        { stdio: ["ignore", "pipe", "ignore"], timeout: 4000 },
      ).toString().trim();
      const bytes = Number(out);
      if (!Number.isFinite(bytes) || bytes <= 0) return null;
      return Math.floor(bytes / (1024 ** 3));
    }
  } catch {
    return null;
  }
  return null;
}

/** Round host RAM to Chrome's deviceMemory bucket {8,16,32}; null → 16. */
export function hostRamBucketGb(): number {
  const gb = hostRamGb();
  if (gb === null) return 16;
  if (gb >= 32) return 32;
  if (gb >= 16) return 16;
  return 8;
}

// ---- Primary monitor (width, height) ----

export type Size = readonly [width: number, height: number];

const MAC_RES_RE = /Resolution:\s*(\d+)\s*x\s*(\d+)/g;
const XRANDR_CUR_RE = /^\s*(\d+)x(\d+)[^\n]*\*/m;

/** Primary monitor (width, height) in CSS pixels, or null on failure. */
export function hostScreenSize(): Size | null {
  const plat = osPlatform();
  try {
    if (plat === "darwin")  return macScreenSize();
    if (plat === "win32")   return windowsScreenSize();
    if (plat === "linux")   return linuxScreenSize();
  } catch {
    return null;
  }
  return null;
}

function macScreenSize(): Size | null {
  // `system_profiler SPDisplaysDataType` reports panel resolution, matching
  // what the launcher's primary_monitor() returns post-scale.
  let out: string;
  try {
    out = execFileSync("system_profiler", ["SPDisplaysDataType"],
      { stdio: ["ignore", "pipe", "ignore"], timeout: 4000 }).toString();
  } catch {
    return null;
  }
  let best: Size | null = null;
  for (const m of out.matchAll(MAC_RES_RE)) {
    const w = Number(m[1]); const h = Number(m[2]);
    if (!Number.isFinite(w) || !Number.isFinite(h)) continue;
    if (best === null || w * h > best[0] * best[1]) best = [w, h];
  }
  return best;
}

function windowsScreenSize(): Size | null {
  try {
    const out = execSync(
      'powershell -NoProfile -Command ' +
      '"Add-Type -AssemblyName System.Windows.Forms; ' +
      '$s=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; ' +
      '\\"$($s.Width)x$($s.Height)\\""',
      { stdio: ["ignore", "pipe", "ignore"], timeout: 4000 },
    ).toString().trim();
    const m = /^(\d+)x(\d+)$/.exec(out);
    if (m) {
      const w = Number(m[1]); const h = Number(m[2]);
      if (w > 0 && h > 0) return [w, h];
    }
  } catch {
    return null;
  }
  return null;
}

function linuxScreenSize(): Size | null {
  let out: string;
  try {
    out = execFileSync("xrandr", ["--query"],
      { stdio: ["ignore", "pipe", "ignore"], timeout: 4000 }).toString();
  } catch {
    return null;
  }
  const m = XRANDR_CUR_RE.exec(out);
  if (m) {
    const w = Number(m[1]); const h = Number(m[2]);
    if (w > 0 && h > 0) return [w, h];
  }
  return null;
}
