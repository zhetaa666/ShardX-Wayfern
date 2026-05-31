// Per-launch randomisation of hardware_concurrency / device_memory /
// platform_version. Mirrors `randomize_hardware` and
// `randomize_platform_version` in `src-tauri/src/lib.rs`.
//
// Mac profiles use the curated MAC_HW_CONFIGS table. Windows / Linux
// profiles use the launcher's host-bracketed logic: real x86 logical-core
// counts filtered to [host_cores-4, host_cores+2], device_memory floored
// by core count and ceilinged by hostRamBucketGb().
import { hostLogicalCores, hostRamBucketGb } from "./host.js";

export const MACOS_PLATFORM_VERSIONS: readonly string[] = [
  "14.6.1", "14.7", "14.7.1", "14.7.2",
  "15.4", "15.4.1", "15.5", "15.6", "15.6.1", "15.7",
  "26.0", "26.0.1", "26.1",
];

export const WINDOWS_PLATFORM_VERSIONS: readonly string[] = [
  "10.0.0",
  "13.0.0",
  "14.0.0", "14.0.0", "14.0.0",
  "15.0.0", "15.0.0", "15.0.0", "15.0.0",
  "16.0.0", "16.0.0", "16.0.0",
  "17.0.0",
];

export const LINUX_PLATFORM_VERSIONS: readonly string[] = [
  "5.15.0", "6.1.0", "6.5.0",
  "6.6.0", "6.8.0", "6.10.0", "6.11.0", "6.12.0",
  "6.14.0", "6.15.0", "6.16.0",
];

type HwPair = readonly [cores: number, gib: number];

export const MAC_HW_CONFIGS: Readonly<Record<string, readonly HwPair[]>> = {
  "mac-m1-air13":     [[8, 8], [8, 16]],
  "mac-m1-mbp13":     [[8, 8], [8, 16]],
  "mac-m1-imac24":    [[8, 8], [8, 16]],
  "mac-m1-pro-mbp14": [[8, 16], [10, 16], [10, 32]],
  "mac-m1-pro-mbp16": [[8, 16], [10, 16], [10, 32]],
  "mac-m1-max-mbp14": [[10, 32]],
  "mac-m1-max-mbp16": [[10, 32]],
  "mac-m2-air13":     [[8, 8], [8, 16]],
  "mac-m2-air15":     [[8, 8], [8, 16]],
  "mac-m2-mbp13":     [[8, 8], [8, 16]],
  "mac-m2-pro-mbp14": [[10, 16], [12, 16], [12, 32]],
  "mac-m2-pro-mbp16": [[10, 16], [12, 16], [12, 32]],
  "mac-m2-max-mbp14": [[12, 32]],
  "mac-m2-max-mbp16": [[12, 32]],
  "mac-m3-air13":     [[8, 8], [8, 16]],
  "mac-m3-air15":     [[8, 8], [8, 16]],
  "mac-m3-mbp14":     [[8, 8], [8, 16]],
  "mac-m3-imac24":    [[8, 8], [8, 16]],
  "mac-m3-pro-mbp14": [[11, 16], [12, 16], [12, 32]],
  "mac-m3-pro-mbp16": [[11, 16], [12, 16], [12, 32]],
  "mac-m3-max-mbp14": [[14, 32], [16, 32]],
  "mac-m3-max-mbp16": [[14, 32], [16, 32]],
  "mac-m4-air13":     [[10, 16], [10, 32]],
  "mac-m4-air15":     [[10, 16], [10, 32]],
  "mac-m4-mbp14":     [[10, 16], [10, 32]],
  "mac-m4-imac24":    [[10, 16], [10, 32]],
  "mac-m4-pro-mbp14": [[12, 16], [14, 16], [14, 32]],
  "mac-m4-pro-mbp16": [[12, 16], [14, 16], [14, 32]],
  "mac-m4-max-mbp14": [[14, 32], [16, 32]],
  "mac-m4-max-mbp16": [[14, 32], [16, 32]],
  "mac-m5-mbp14":     [[10, 16], [10, 32]],
};

/** Real x86 logical-core counts (SMT + Intel hybrid). Same array as lib.rs. */
export const X86_CORES: readonly number[] = [4, 6, 8, 12, 16, 20, 24, 28, 32];

function pickRandom<T>(pool: readonly T[]): T {
  return pool[Math.floor(Math.random() * pool.length)];
}

function platformOf(cfg: Record<string, unknown>): string {
  const nav = cfg["navigator"] as Record<string, unknown> | undefined;
  return (nav?.["platform"] as string | undefined) ?? "";
}

/** Mutates in-place: pick fresh `navigator.platform_version` (+ mirror to client_hints). */
export function randomizePlatformVersion(cfg: Record<string, unknown>): void {
  const plat = platformOf(cfg);
  const pool =
    plat === "macOS"   ? MACOS_PLATFORM_VERSIONS :
    plat === "Windows" ? WINDOWS_PLATFORM_VERSIONS :
    plat === "Linux"   ? LINUX_PLATFORM_VERSIONS : null;
  if (!pool) return;
  const v = pickRandom(pool);
  const nav = (cfg["navigator"] ??= {}) as Record<string, unknown>;
  nav["platform_version"] = v;
  const ch = cfg["client_hints"] as Record<string, unknown> | undefined;
  if (ch && typeof ch === "object") ch["platform_version"] = v;
}

/**
 * Mutates in-place: pick fresh (hardware_concurrency, device_memory).
 *
 * macOS: curated MAC_HW_CONFIGS table by profile id.
 * Windows / Linux: bracket the host CPU count within [C-4, C+2] from
 * X86_CORES; floor device_memory by core count (>=12 → 16, else 8) and
 * cap by `hostRamBucketGb()`.
 */
export function randomizeHardware(cfg: Record<string, unknown>, profileId?: string): void {
  const plat = platformOf(cfg);
  let cores: number;
  let mem: number;

  if (plat === "macOS" && profileId && profileId in MAC_HW_CONFIGS) {
    [cores, mem] = pickRandom(MAC_HW_CONFIGS[profileId]);
  } else if (plat === "Windows" || plat === "Linux") {
    const c = hostLogicalCores();
    const lo = Math.max(0, c - 4);
    const hi = c + 2;
    const candidates = X86_CORES.filter((n) => n >= lo && n <= hi);
    if (candidates.length > 0) {
      cores = pickRandom(candidates);
    } else {
      cores = X86_CORES.reduce((best, n) =>
        Math.abs(n - c) < Math.abs(best - c) ? n : best, X86_CORES[0]);
    }
    const real = hostRamBucketGb();
    const floor = cores >= 12 ? 16 : 8;
    const memCand = [8, 16, 32].filter((m) => m >= floor && m <= real);
    mem = memCand.length > 0 ? pickRandom(memCand) : real;
  } else {
    return;
  }

  const nav = (cfg["navigator"] ??= {}) as Record<string, unknown>;
  nav["hardware_concurrency"] = cores;
  nav["device_memory"] = mem;
}
