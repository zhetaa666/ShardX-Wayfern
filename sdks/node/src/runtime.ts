// Runtime cache: download ShardX engine + Widevine CDM + fingerprint
// library from the ProxyShard CDN, extract into a per-user cache dir,
// place Widevine inside the engine bundle, remember etags so subsequent
// runs are zero-network. Mirrors src-tauri/src/runtime.rs in the launcher.
import { createWriteStream, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync, chmodSync, copyFileSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { homedir, platform as osPlatform, arch as osArch } from "node:os";
import { join, dirname, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { spawnSync } from "node:child_process";
import AdmZip from "adm-zip";

export const PUB_BASE = "https://pub-e57a7c60f6934eb09a6600bf2fc59cdc.r2.dev";
export const CHROMIUM_VERSION = "148.0.7778.216";

export function defaultCacheDir(): string {
  const plat = osPlatform();
  if (plat === "darwin") return join(homedir(), "Library", "Application Support", "shardx-sdk");
  if (plat === "win32")  return join(process.env.LOCALAPPDATA ?? homedir(), "shardx-sdk");
  return join(process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), "shardx-sdk");
}

export interface Archive { key: string; label: string; }

export interface HostSpec {
  browser: Archive;
  widevine: Archive | null;
  binarySubpath: string[];
  widevineSubpath: string[];
}

export function hostSpec(): HostSpec {
  const plat = osPlatform();
  const arch = osArch();
  if (plat === "darwin" && arch === "arm64") {
    return {
      browser:  { key: "ShardX-Mac-arm64.zip",          label: "ShardX browser (macOS arm64)" },
      widevine: { key: "ShardX-Widevine-Mac-arm64.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Mac-arm64", "ShardX.app", "Contents", "MacOS", "ShardX"],
      widevineSubpath: ["ShardX-Mac-arm64", "ShardX.app", "Contents", "Frameworks",
                        "ShardX Framework.framework", "Versions", CHROMIUM_VERSION,
                        "Libraries", "WidevineCdm"],
    };
  }
  if (plat === "win32" && arch === "x64") {
    return {
      browser:  { key: "ShardX-Windows.zip",     label: "ShardX browser (Windows x64)" },
      widevine: { key: "ShardX-Widevine-Win.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Windows", "chrome.exe"],
      widevineSubpath: ["ShardX-Windows", "WidevineCdm"],
    };
  }
  if (plat === "linux" && arch === "x64") {
    return {
      browser:  { key: "ShardX-Linux.zip",         label: "ShardX browser (Linux x64)" },
      widevine: { key: "ShardX-Widevine-Linux.zip", label: "Widevine CDM" },
      binarySubpath:   ["ShardX-Linux", "chrome"],
      widevineSubpath: ["ShardX-Linux", "WidevineCdm"],
    };
  }
  throw new Error(`Unsupported host: ${plat}/${arch}. ShardX ships mac-arm64, win-x64, linux-x64.`);
}

export const FINGERPRINTS_ARCHIVE: Archive = {
  key: "ShardX-Fingerprints.zip",
  label: "Fingerprint library",
};
const FINGERPRINTS_TOP_DIR = "shardx-fingerprints";

export type ProgressCb = (label: string, received: number, total: number) => void;

interface Manifest {
  browser_etag?: string;
  widevine_etag?: string;
  fingerprints_etag?: string;
}

export class Runtime {
  readonly root: string;
  readonly spec: HostSpec;
  private readonly progress?: ProgressCb;
  private readonly _profilesRoot?: string;
  /** Set after a successful in-process install() so subsequent launches
   *  skip the R2 HEAD round-trip (~1 s over a clean connection).  Cleared
   *  by `install({force: true})`. */
  private _checkedInProcess = false;

  constructor(opts: { cacheDir?: string; progress?: ProgressCb; profilesDir?: string } = {}) {
    this.root = opts.cacheDir ?? defaultCacheDir();
    mkdirSync(this.root, { recursive: true });
    this._profilesRoot = opts.profilesDir ? resolve(opts.profilesDir) : undefined;
    this.progress = opts.progress;
    this.spec = hostSpec();
  }

  get manifestPath(): string  { return join(this.root, "manifest.json"); }
  get binaryPath(): string    { return join(this.root, ...this.spec.binarySubpath); }
  get fingerprintsDir(): string {
    const d = join(this.root, "fingerprints");
    mkdirSync(d, { recursive: true });
    return d;
  }
  /** Per-profile user-data-dir root. Defaults to `<cacheDir>/profiles/`;
   *  override via `new ShardX({ profilesDir })` or per-launch
   *  `userDataDir`. Resolved path is logged at launch time. */
  get profilesRoot(): string {
    const d = this._profilesRoot ?? join(this.root, "profiles");
    mkdirSync(d, { recursive: true });
    return d;
  }
  get installed(): boolean    { return existsSync(this.binaryPath); }

  // ---- manifest ----

  private loadManifest(): Manifest {
    try { return JSON.parse(readFileSync(this.manifestPath, "utf8")); }
    catch { return {}; }
  }
  private saveManifest(m: Manifest): void {
    writeFileSync(this.manifestPath, JSON.stringify(m, null, 2));
  }

  // ---- install ----

  async install(opts: { force?: boolean } = {}): Promise<void> {
    const force = !!opts.force;
    if (this._checkedInProcess && !force) return;
    const local = this.loadManifest();

    const remoteBrowser = await this.headEtag(this.spec.browser.key);
    const needBrowser = force || !this.installed || local.browser_etag !== remoteBrowser;
    if (needBrowser) {
      const etag = await this.downloadAndExtract(this.spec.browser, this.root);
      local.browser_etag = etag;
    }
    if (this.spec.widevine && (needBrowser || !local.widevine_etag)) {
      const etag = await this.downloadAndExtract(this.spec.widevine, this.root);
      this.placeWidevine();
      local.widevine_etag = etag;
    }
    const remoteFp = await this.headEtag(FINGERPRINTS_ARCHIVE.key);
    const fpDirHasJson = readdirSync(this.fingerprintsDir).some((f) => f.endsWith(".json"));
    if (force || local.fingerprints_etag !== remoteFp || !fpDirHasJson) {
      await this.installFingerprints(force);
      if (remoteFp) local.fingerprints_etag = remoteFp;
    }
    this.saveManifest(local);

    if (osPlatform() !== "win32" && existsSync(this.binaryPath)) {
      const m = statSync(this.binaryPath).mode;
      chmodSync(this.binaryPath, m | 0o111);
    }
    this._checkedInProcess = true;
  }

  // ---- helpers ----

  private async headEtag(key: string): Promise<string | undefined> {
    try {
      const r = await fetch(`${PUB_BASE}/${key}`, { method: "HEAD" });
      if (!r.ok) return undefined;
      return r.headers.get("etag")?.replace(/^"|"$/g, "") ?? undefined;
    } catch { return undefined; }
  }

  private async downloadAndExtract(arch: Archive, dest: string): Promise<string> {
    const url = `${PUB_BASE}/${arch.key}`;
    mkdirSync(dest, { recursive: true });
    const tmp = join(dest, `.${arch.key}.tmp`);

    const r = await fetch(url);
    if (!r.ok || !r.body) throw new Error(`download ${arch.key}: HTTP ${r.status}`);
    const etag = r.headers.get("etag")?.replace(/^"|"$/g, "") ?? "";
    const total = Number(r.headers.get("content-length") ?? 0);

    let received = 0;
    const reader = r.body.getReader();
    const out = createWriteStream(tmp);
    const stream = new Readable({
      async read() {
        const { value, done } = await reader.read();
        if (done) { this.push(null); return; }
        received += value.byteLength;
        if (arch.label) {/* throttle: rounded percent */}
        this.push(Buffer.from(value));
      },
    });
    // Wire progress in a parallel listener so pipeline stays clean.
    if (this.progress) {
      stream.on("data", () => this.progress!(arch.label, received, total));
    }
    await pipeline(stream, out);

    // Extract.  IMPORTANT: on macOS/Linux shell out to the system
    // `unzip` instead of adm-zip — adm-zip writes symlinks as ordinary
    // text files (every `Versions/Current/...` link in a `.app`
    // framework becomes a 24-byte regular file) and drops the +x bit
    // on every helper executable.  The result extracts cleanly but
    // fails to launch — GPU helper can't find the framework dylib.
    if (osPlatform() === "win32") {
      new AdmZip(tmp).extractAllTo(dest, /*overwrite*/ true);
    } else {
      systemUnzip(tmp, dest);
    }
    rmSync(tmp, { force: true });
    return etag;
  }

  private placeWidevine(): void {
    if (!this.spec.widevine) return;
    const wrapper = this.spec.widevine.key.replace(/\.zip$/, "");
    const src = join(this.root, wrapper, "WidevineCdm");
    if (!existsSync(src)) return;
    const dst = join(this.root, ...this.spec.widevineSubpath);
    if (existsSync(dst)) rmSync(dst, { recursive: true, force: true });
    mkdirSync(dirname(dst), { recursive: true });
    renameSync(src, dst);
    rmSync(join(this.root, wrapper), { recursive: true, force: true });
  }

  private async installFingerprints(force: boolean): Promise<void> {
    const url = `${PUB_BASE}/${FINGERPRINTS_ARCHIVE.key}`;
    const staging = join(this.fingerprintsDir, ".staging");
    if (existsSync(staging)) rmSync(staging, { recursive: true, force: true });
    mkdirSync(staging, { recursive: true });
    const tmp = join(staging, "bundle.zip");

    const r = await fetch(url);
    if (!r.ok || !r.body) throw new Error(`download fingerprints: HTTP ${r.status}`);
    const total = Number(r.headers.get("content-length") ?? 0);

    let received = 0;
    const reader = r.body.getReader();
    const out = createWriteStream(tmp);
    const stream = new Readable({
      async read() {
        const { value, done } = await reader.read();
        if (done) { this.push(null); return; }
        received += value.byteLength;
        this.push(Buffer.from(value));
      },
    });
    if (this.progress) {
      stream.on("data", () => this.progress!(FINGERPRINTS_ARCHIVE.label, received, total));
    }
    await pipeline(stream, out);

    // Fingerprints bundle is plain JSON files — adm-zip is fine here
    // (no symlinks / exec bits to preserve).
    new AdmZip(tmp).extractAllTo(staging, true);

    const srcDir = join(staging, FINGERPRINTS_TOP_DIR);
    const walk = existsSync(srcDir) ? srcDir : staging;
    for (const name of readdirSync(walk)) {
      if (!name.endsWith(".json")) continue;
      const dst = join(this.fingerprintsDir, name);
      if (force || !existsSync(dst)) copyFileSync(join(walk, name), dst);
    }
    rmSync(staging, { recursive: true, force: true });
  }
}

/** Extract via /usr/bin/unzip — preserves symlinks and permission
 *  bits that adm-zip silently drops.  Required for any macOS .app
 *  bundle (Versions/Current symlinks + Helper exec bits). */
function systemUnzip(archive: string, dest: string): void {
  mkdirSync(dest, { recursive: true });
  const r = spawnSync("unzip", ["-q", "-o", archive, "-d", dest], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  if (r.error) {
    if ((r.error as NodeJS.ErrnoException).code === "ENOENT") {
      throw new Error(
        "system `unzip` not found — required for symlink-preserving extraction on macOS / Linux",
      );
    }
    throw r.error;
  }
  if (r.status !== 0) {
    const err = r.stderr?.toString().slice(0, 400) ?? `exit ${r.status}`;
    throw new Error(`unzip failed for ${archive}: ${err}`);
  }
}
