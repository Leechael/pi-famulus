/** Locations of the system under test. Everything is overridable by env. */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const EVAL_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const REPO_ROOT = resolve(EVAL_DIR, "..");

/** Extension under test (a pi package dir). */
export const EXTENSION_DIR = process.env.PI_FAMULUS_EVAL_EXTENSION ?? join(REPO_ROOT, "extension");
export const FAUX_EXT = join(EVAL_DIR, "harness", "faux-ext.ts");
export const ABLATION_EXT = join(EVAL_DIR, "harness", "ablation-ext.ts");
export const PI_BIN = process.env.PI_BIN ?? "pi";

/**
 * Program and leading arguments that run pi. A `.js` PI_BIN (pi's
 * `dist/bundle/cli.js`) runs under this Node: on Windows `node_modules/.bin/pi`
 * is a `.cmd` shim, which `spawn` cannot start without a shell.
 */
export function piCommand(args: string[]): [string, string[]] {
  return /\.[cm]?js$/.test(PI_BIN) ? [process.execPath, [PI_BIN, ...args]] : [PI_BIN, args];
}

const CACHE_TARGET = join(EVAL_DIR, ".cache", "target");
const BUILT_MANAGER = join(CACHE_TARGET, "release", "pi-famulus");

/**
 * pi-famulus binary: $PI_FAMULUS_MANAGER_PATH, else built from this repo's manager/
 * into eval/.cache/target (never writes inside manager/).
 */
let managerBuilt = false;

export function managerPath(): string {
  const explicit = process.env.PI_FAMULUS_MANAGER_PATH;
  if (explicit) return explicit;
  // Always run an incremental cargo build once per process: a cached binary
  // from older sources silently tests the wrong manager (it once masked a
  // `sessions` output change). A no-op rebuild takes well under a second.
  if (!managerBuilt) {
    managerBuilt = true;
    const r = spawnSync(
      "cargo",
      ["build", "--release", "--manifest-path", join(REPO_ROOT, "manager", "Cargo.toml"), "--target-dir", CACHE_TARGET],
      { stdio: "inherit" },
    );
    if (r.status !== 0 || !existsSync(BUILT_MANAGER)) throw new Error("failed to build pi-famulus");
  }
  return BUILT_MANAGER;
}
