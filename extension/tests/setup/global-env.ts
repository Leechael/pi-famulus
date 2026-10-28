/**
 * Once per run, in the main process (vitest.config.ts `globalSetup`): read
 * what the host looks like, build the tool-free PATH, and hand both to
 * tests/setup/hermetic-env.ts, which isolates each test file.
 */
import { existsSync, mkdirSync, mkdtempSync, readdirSync, realpathSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir, userInfo } from "node:os";
import { delimiter, join, resolve } from "node:path";
import type { TestProject } from "vitest/node";

export interface HermeticEnv {
  /** Every test file's sandbox is created in here; removed after the run. */
  root: string;
  /** PATH without pi-famulus, pi, claude, codex or herdr. */
  path: string;
  /** The developer's home, by every route a lookup could take. */
  realHomes: string[];
  /** Opt-in test inputs that survive isolation. */
  keep: Record<string, string>;
}

declare module "vitest" {
  export interface ProvidedContext {
    hermeticEnv: HermeticEnv;
  }
}

const WINDOWS = process.platform === "win32";
const HOST_TOOLS = ["pi-famulus", "pi", "claude", "codex", "herdr"];

export default function setup(project: TestProject): () => void {
  // Short: the default messaging socket path under it must fit sun_path.
  const root = mkdtempSync(join(tmpdir(), "pf-env-"));
  const env = process.env;
  const keep: Record<string, string> = {};
  for (const key of ["PI_FAMULUS_INTEG", "PI_FAMULUS_TEST_ARTIFACTS"]) if (env[key] !== undefined) keep[key] = env[key];
  // It names the binary under test there; in a daily shell it names the user's install.
  if (env.PI_FAMULUS_INTEG === "1" && env.PI_FAMULUS_MANAGER_PATH) keep.PI_FAMULUS_MANAGER_PATH = env.PI_FAMULUS_MANAGER_PATH;
  const realHomes = [env.HOME, env.USERPROFILE, userHome()].filter((dir): dir is string => Boolean(dir)).map(canonical);
  const pathKey = Object.keys(env).find((key) => key.toUpperCase() === "PATH") ?? "PATH";
  project.provide("hermeticEnv", {
    root,
    path: toolFreePath(env[pathKey] ?? "", join(root, "path")),
    realHomes: [...new Set(realHomes)],
    keep,
  });
  return () => rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}

function userHome(): string | undefined {
  try {
    return userInfo().homedir;
  } catch {
    return undefined;
  }
}

function canonical(dir: string): string {
  try {
    return realpathSync.native(dir);
  } catch {
    return resolve(dir);
  }
}

function hasHostTool(dir: string): boolean {
  const names = WINDOWS ? HOST_TOOLS.flatMap((tool) => [`${tool}.exe`, `${tool}.cmd`, `${tool}.ps1`, tool]) : HOST_TOOLS;
  return names.some((name) => existsSync(join(dir, name)));
}

/**
 * A directory holding one of the host's tools is replaced by a directory of
 * links to everything else in it (dropped on Windows, where links need
 * privileges). Tests that need a tool put their own on PATH.
 */
function toolFreePath(path: string, farmRoot: string): string {
  const entries: string[] = [];
  for (const [index, dir] of path.split(delimiter).entries()) {
    if (!dir || !hasHostTool(dir)) {
      entries.push(dir);
      continue;
    }
    if (WINDOWS) continue;
    const farm = join(farmRoot, String(index));
    mkdirSync(farm, { recursive: true });
    let names: string[] = [];
    try {
      names = readdirSync(dir);
    } catch {
      // An unreadable directory contributes nothing.
    }
    for (const name of names) {
      if (HOST_TOOLS.includes(name)) continue;
      try {
        symlinkSync(join(dir, name), join(farm, name));
      } catch {
        // Duplicate or vanished entries are simply not on the test PATH.
      }
    }
    entries.push(farm);
  }
  return entries.join(delimiter);
}
