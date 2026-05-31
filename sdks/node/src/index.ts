// Top-level façade — bundles the runtime, fingerprint library, and
// browser launcher. Mirrors the Python `ShardX` class.
import { chromium, type Browser as PatchrightBrowser } from "patchright";

import { Runtime, type ProgressCb } from "./runtime.js";
import { FingerprintLibrary, Profile } from "./profile.js";
import { Browser, type LaunchOptions, type BrowserSession } from "./browser.js";
import { randomizeHardware, randomizePlatformVersion } from "./randomize.js";
import { parseProxy, probeUdp } from "./proxy.js";
import { geoCheckVia, type GeoInfo } from "./geo.js";

export interface ShardXOptions {
  /** Where the engine, Widevine, and bundled fingerprint library live
   *  (defaults to the per-OS app-data dir). */
  cacheDir?: string;
  progress?: ProgressCb;
  /** Per-profile user-data-dir root (cookies, IndexedDB, cache).
   *  Defaults to `./shardx-profiles/` next to the running script. */
  profilesDir?: string;
}

export type LaunchInput = string | Profile | Record<string, unknown>;

export interface ShardXLaunchOptions extends LaunchOptions {
  /** When `fingerprint` is omitted, pick a random profile filtered by this platform substring. */
  platform?: string;
  /** When true, re-pick hardware_concurrency / device_memory / platform_version before launch. */
  randomize?: boolean;
}

export interface ProxyCheckResult {
  udpMs: number | null;
  geo: GeoInfo;
  wouldEnableQuic: boolean;
  wouldSetWebrtc: "auto" | "tcp_only";
}

export class ShardX {
  readonly runtime: Runtime;
  readonly library: FingerprintLibrary;
  private readonly browser: Browser;

  constructor(opts: ShardXOptions = {}) {
    this.runtime = new Runtime(opts);
    this.library = new FingerprintLibrary(this.runtime);
    this.browser = new Browser(this.runtime);
  }

  /** All bundled fingerprint ids, optionally filtered by `navigator.platform`.
   *  Auto-installs the fingerprint library on first call. */
  async listProfiles(opts: { platform?: string } = {}): Promise<string[]> {
    await this.runtime.install();
    return opts.platform ? Array.from(this.library.filter({ platform: opts.platform })) : this.library.ids();
  }

  /** Pick a random profile from the library.  Auto-installs on first call. */
  async randomProfile(opts: { platform?: string } = {}): Promise<Profile> {
    const ids = await this.listProfiles(opts);
    if (ids.length === 0) {
      throw new Error(`No bundled profiles found${opts.platform ? ` for platform=${opts.platform}` : ""}. Did you call ensureInstalled()?`);
    }
    return this.library.load(ids[Math.floor(Math.random() * ids.length)]);
  }

  /**
   * Launch a profile.
   *
   * @param fingerprint  Profile id, `Profile` instance, plain dict, or `null`/omitted to pick random.
   * @param opts.platform  When picking random, filter by `navigator.platform` substring.
   * @param opts.randomize When true, freshly randomise hw_concurrency / device_memory / platform_version.
   *                       (Same logic the desktop launcher applies when re-picking a GPU.)
   * All other options forwarded to `Browser.launch` (proxy, cdp, headless, webrtc, screenMode, …).
   */
  async launch(fingerprint?: LaunchInput | null, opts: ShardXLaunchOptions = {}): Promise<BrowserSession> {
    await this.runtime.install();
    let profile: Profile;
    if (fingerprint == null) {
      profile = await this.randomProfile({ platform: opts.platform });
    } else if (typeof fingerprint === "string") {
      profile = this.library.load(fingerprint);
    } else if (fingerprint instanceof Profile) {
      profile = fingerprint;
    } else if (typeof fingerprint === "object") {
      profile = new Profile(fingerprint as Record<string, unknown>);
    } else {
      throw new TypeError(`fingerprint must be string | Profile | object | null; got ${typeof fingerprint}`);
    }
    if (opts.randomize) {
      randomizeHardware(profile.config, profile.id);
      randomizePlatformVersion(profile.config);
    }
    const { platform: _p, randomize: _r, ...launchOpts } = opts;
    return this.browser.launch(profile, launchOpts);
  }

  /**
   * Launch a profile AND connect patchright in one call.  Returns an
   * object with the patchright `Browser`, the raw `BrowserSession`, and
   * a `close()` that tears both down.
   *
   * Requires `patchright` (`npm install patchright`) as an optional
   * peer-dependency.
   *
   * @example
   * const { browser, close } = await sdk.session({ fingerprint: "win-rtx4060", proxy: "socks5://…" });
   * try {
   *   const page = await browser.contexts()[0].newPage();
   *   await page.goto("https://example.com");
   * } finally {
   *   await close();
   * }
   */
  async session(opts: ShardXLaunchOptions & { fingerprint?: LaunchInput | null } = {}): Promise<{
    browser: PatchrightBrowser;
    session: BrowserSession;
    close: () => Promise<void>;
  }> {
    const { fingerprint, ...launchOpts } = opts;
    const sess = await this.launch(fingerprint ?? null, { ...launchOpts, cdp: true });
    if (!sess.cdpUrl) {
      await sess.stop();
      throw new Error("CDP endpoint unavailable — engine failed to expose remote-debugging port");
    }
    const browser = await chromium.connectOverCDP(sess.cdpUrl);
    return {
      browser,
      session: sess,
      async close() {
        try { await browser.close(); } catch { /* ignore */ }
        await sess.stop();
      },
    };
  }

  /**
   * Validate a proxy URL before binding it to a profile. Returns the same
   * data the launcher uses to decide QUIC + WebRTC policy.
   */
  async checkProxy(proxyUrl: string): Promise<ProxyCheckResult> {
    const parsed = parseProxy(proxyUrl);
    const udpMs = parsed.scheme === "socks5" ? await probeUdp(parsed) : null;
    const geo = await geoCheckVia(parsed);
    const udpOk = udpMs !== null;
    return {
      udpMs,
      geo,
      wouldEnableQuic: udpOk,
      wouldSetWebrtc: udpOk ? "auto" : "tcp_only",
    };
  }
}

export { Runtime, defaultCacheDir, PUB_BASE, CHROMIUM_VERSION, hostSpec } from "./runtime.js";
export type { ProgressCb, HostSpec, Archive } from "./runtime.js";
export { Profile, FingerprintLibrary, userDataDir } from "./profile.js";
export { Browser, BrowserSession } from "./browser.js";
export type { LaunchOptions, WebRtcMode, ScreenMode } from "./browser.js";
export { parseProxy, probeUdp, proxyToArg } from "./proxy.js";
export type { ParsedProxy } from "./proxy.js";
export {
  randomizeHardware, randomizePlatformVersion,
  MAC_HW_CONFIGS, X86_CORES,
  MACOS_PLATFORM_VERSIONS, WINDOWS_PLATFORM_VERSIONS, LINUX_PLATFORM_VERSIONS,
} from "./randomize.js";
export {
  hostLogicalCores, hostRamGb, hostRamBucketGb, hostScreenSize,
} from "./host.js";
export type { Size } from "./host.js";
export { applyScreenStrategy, defaultScreenModeFor } from "./screen.js";
export type { ScreenStrategy } from "./screen.js";
export { geoCheckVia } from "./geo.js";
export type { GeoInfo, GeoProvider } from "./geo.js";
export { hasAutoFields, resolveAutoFields } from "./autoResolve.js";
