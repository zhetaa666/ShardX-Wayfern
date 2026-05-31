// Profile = a fingerprint JSON + a per-launch working dir. Wraps the
// bundled fingerprint library and lets callers override fields before
// launch.
import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { Runtime } from "./runtime.js";

export class Profile {
  readonly id: string;
  config: Record<string, unknown>;

  constructor(config: Record<string, unknown>, id?: string) {
    this.config = JSON.parse(JSON.stringify(config));   // deep clone
    this.id = id ?? (config["name"] as string | undefined) ?? "anonymous";
  }

  static fromFile(path: string): Profile {
    const cfg = JSON.parse(readFileSync(path, "utf8"));
    const id = path.split(/[\\/]/).pop()!.replace(/\.json$/, "");
    return new Profile(cfg, id);
  }

  /** Shallow merge: object values are merged one level deep, scalars replaced. */
  withOverride(overrides: Record<string, unknown>): Profile {
    const out: Record<string, unknown> = JSON.parse(JSON.stringify(this.config));
    for (const [k, v] of Object.entries(overrides)) {
      if (v && typeof v === "object" && !Array.isArray(v)
          && out[k] && typeof out[k] === "object" && !Array.isArray(out[k])) {
        out[k] = { ...(out[k] as object), ...(v as object) };
      } else {
        out[k] = v;
      }
    }
    return new Profile(out, (overrides["name"] as string | undefined) ?? this.id);
  }

  get platform(): string {
    const nav = this.config["navigator"] as Record<string, unknown> | undefined;
    return (nav?.["platform"] as string | undefined) ?? "";
  }

  get hasWebGPU(): boolean {
    const wgp = this.config["webgpu"] as Record<string, unknown> | null | undefined;
    if (!wgp) return false;
    const limits = wgp["limits"];
    return !!(limits && typeof limits === "object" && Object.keys(limits as object).length > 0);
  }
}

export class FingerprintLibrary {
  constructor(private readonly runtime: Runtime) {}

  ids(): string[] {
    return readdirSync(this.runtime.fingerprintsDir)
      .filter((f) => f.endsWith(".json"))
      .map((f) => f.replace(/\.json$/, ""))
      .sort();
  }

  *filter(opts: { platform?: string } = {}): Generator<string> {
    for (const id of this.ids()) {
      if (opts.platform) {
        try {
          const p = this.load(id);
          if (!p.platform.toLowerCase().includes(opts.platform.toLowerCase())) continue;
        } catch { continue; }
      }
      yield id;
    }
  }

  load(fingerprintId: string): Profile {
    const path = join(this.runtime.fingerprintsDir, `${fingerprintId}.json`);
    if (!existsSync(path)) {
      const sample = this.ids().slice(0, 10).join(", ");
      throw new Error(`Fingerprint '${fingerprintId}' not found. Available: ${sample}…`);
    }
    return Profile.fromFile(path);
  }
}

/** Per-profile state (cookies / IndexedDB / cache) — preserved across
 *  launches. Defaults to `./shardx-profiles/<id>/` next to the running
 *  script. Override per launch with `userDataDir` or per SDK with
 *  `new ShardX({ profilesDir })`. */
export function userDataDir(runtime: Runtime, profileId: string, base?: string): string {
  const root = base ?? runtime.profilesRoot;
  const d = join(root, profileId);
  mkdirSync(d, { recursive: true });
  return d;
}
