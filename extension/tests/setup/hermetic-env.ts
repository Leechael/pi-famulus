/**
 * Every test file runs against an environment this file decides, not the
 * developer's machine (vitest.config.ts `setupFiles`, once per test file,
 * before the file is imported; tests/setup/global-env.ts reads the host).
 *
 * - HOME / USERPROFILE and every per-user dir (XDG_*, LOCALAPPDATA, APPDATA)
 *   point at an empty per-file sandbox, so os.homedir(), the Famulus home,
 *   pi's agent dir and the native package cache never reach the real ones
 *   (or the real daemon's socket under ~/.pi/agent/pi-famulus). So does
 *   XDG_RUNTIME_DIR, where the default messaging client socket lives.
 * - Inherited PI_* settings, proxies, colour/terminal and multiplexer vars
 *   are dropped; a test that needs one sets it itself. Opt-in test inputs
 *   (PI_FAMULUS_INTEG and friends, see global-env.ts) survive.
 * - PATH keeps everything except pi-famulus, pi, claude, codex and herdr.
 * - TZ is fixed.
 *
 * Children inherit process.env, so they get the same environment.
 *
 * The afterEach guard fails a test that leaves a per-user location pointing
 * anywhere but the sandbox, or at the real home.
 */
import { mkdirSync, mkdtempSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative, isAbsolute, resolve } from "node:path";
import { afterEach, expect, inject } from "vitest";

const DROP_PREFIXES = ["PI_", "HERDR_", "KITTY_", "ITERM_", "WEZTERM_", "GHOSTTY_", "TERM_PROGRAM", "XDG_"];
const DROP = [
  "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "FTP_PROXY",
  "NO_COLOR", "FORCE_COLOR", "NODE_DISABLE_COLORS", "COLORTERM", "COLUMNS", "LINES", "TERMINFO",
  "TMUX", "TMUX_PANE", "STY", "WT_SESSION", "VTE_VERSION", "EDITOR", "VISUAL",
];

const host = inject("hermeticEnv");
const sandbox = mkdtempSync(join(host.root, "f-"));
const home = join(sandbox, "home");
const isolated: Record<string, string> = {
  HOME: home,
  USERPROFILE: home,
  XDG_CACHE_HOME: join(home, ".cache"),
  XDG_CONFIG_HOME: join(home, ".config"),
  XDG_DATA_HOME: join(home, ".local", "share"),
  XDG_STATE_HOME: join(home, ".local", "state"),
  LOCALAPPDATA: join(home, "AppData", "Local"),
  APPDATA: join(home, "AppData", "Roaming"),
  ...(process.platform === "win32" ? {} : { XDG_RUNTIME_DIR: join(sandbox, "run") }),
  TZ: "UTC",
  TERM: "dumb",
};
for (const dir of Object.values(isolated).filter((value) => value.startsWith(sandbox))) {
  mkdirSync(dir, { recursive: true, mode: 0o700 });
}

for (const key of Object.keys(process.env)) {
  const upper = key.toUpperCase();
  if (DROP.includes(upper) || DROP_PREFIXES.some((prefix) => upper.startsWith(prefix))) delete process.env[key];
}
Object.assign(process.env, host.keep, isolated);
const pathKey = Object.keys(process.env).find((key) => key.toUpperCase() === "PATH") ?? "PATH";
process.env[pathKey] = host.path;

afterEach(() => {
  const leaked: string[] = [];
  for (const [key, value] of Object.entries(isolated)) {
    if (process.env[key] !== value) leaked.push(`${key}=${process.env[key] ?? "<unset>"}`);
    process.env[key] = value;
  }
  for (const key of ["PI_FAMULUS_HOME", "PI_CODING_AGENT_DIR", "PI_CODING_AGENT_SESSION_DIR"]) {
    const value = process.env[key];
    if (value && underRealHome(value)) {
      leaked.push(`${key}=${value}`);
      delete process.env[key];
    }
  }
  expect(leaked, "a test left a per-user location outside its sandbox (restore it, or use a temp dir)").toEqual([]);
});

function canonical(dir: string): string {
  try {
    return realpathSync.native(dir);
  } catch {
    return resolve(dir);
  }
}

/** Under a real home and not in the temp dir (on Windows %TEMP% is inside the profile). */
function underRealHome(path: string): boolean {
  const target = canonical(path);
  const inside = (dir: string) => {
    const rel = relative(dir, target);
    return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
  };
  return host.realHomes.some(inside) && !inside(canonical(tmpdir()));
}
