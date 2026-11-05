/**
 * Per-episode sandbox: temp cwd + isolated PI_FAMULUS_HOME (own manager socket,
 * config.json, task logs). pi's own agent dir is NOT isolated, so real-model
 * runs reuse the user's login (see ablation/README section in run.ts --help).
 */
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { managerPath } from "./paths.ts";

export interface SandboxOptions {
  /** Written to $PI_FAMULUS_HOME/config.json. */
  famulusConfig?: Record<string, unknown>;
  /** Files (relative to cwd) to create before the episode. */
  files?: Record<string, string>;
  keep?: boolean;
}

export interface Sandbox {
  root: string;
  cwd: string;
  famulusHome: string;
  tracePath: string;
  env: Record<string, string>;
  cleanup(): void;
}

/**
 * Wait until the extension of a running pi holds a live manager connection.
 * Needed because the extension connects in the background at session_start
 * and a tool call racing that connect loses the connection (see
 * e2e/faux.test.ts "cold start" — known bug).
 */
/** `sessions --json` lists live sessions with state "connected" (older managers: connected: true). */
function hasConnectedSession(stdout: string): boolean {
  try {
    const rows = JSON.parse(stdout) as Array<{ state?: string; connected?: boolean }>;
    return rows.some((row) => row.state === "connected" || row.connected === true);
  } catch {
    return false;
  }
}

export async function waitManagerReady(sb: Sandbox, timeoutMs = 10_000): Promise<void> {
  const mgr = sb.env.PI_FAMULUS_MANAGER_PATH;
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const r = spawnSync(mgr, ["--home", sb.famulusHome, "sessions", "--json"], { encoding: "utf8", timeout: 3000 });
    if (r.status === 0 && hasConnectedSession(r.stdout)) return;
    if (Date.now() > deadline) throw new Error(`pi-famulus not ready: ${r.stdout}${r.stderr}`);
    await new Promise((res) => setTimeout(res, 100));
  }
}

/** Any running shell/monitor task or in-process subagent in this PI_FAMULUS_HOME? */
export function hasRunningWork(sb: Sandbox): boolean {
  const r = spawnSync(sb.env.PI_FAMULUS_MANAGER_PATH, ["--home", sb.famulusHome, "ls"], { encoding: "utf8", timeout: 3000 });
  if (r.status !== 0) return false; // manager gone: nothing can wake the agent any more
  return !/no running tasks/.test(r.stdout);
}

export function createSandbox(opts: SandboxOptions = {}): Sandbox {
  // Short base path: unix socket paths are limited to ~104 bytes on macOS.
  const root = mkdtempSync(join(process.platform === "darwin" ? "/tmp" : tmpdir(), "eval-"));
  const cwd = join(root, "w");
  const famulusHome = join(root, "h");
  // Per-episode TMPDIR so concurrent episodes do not share mktemp/$TMPDIR files.
  const tmp = join(root, "t");
  mkdirSync(cwd, { recursive: true });
  mkdirSync(famulusHome, { recursive: true });
  mkdirSync(tmp, { recursive: true });
  const mgr = managerPath();
  writeFileSync(
    join(famulusHome, "config.json"),
    JSON.stringify({ managerPath: mgr, ...(opts.famulusConfig ?? {}) }, null, 2),
  );
  for (const [rel, content] of Object.entries(opts.files ?? {})) {
    const p = join(cwd, rel);
    mkdirSync(dirname(p), { recursive: true });
    writeFileSync(p, content);
  }
  const tracePath = join(root, "faux-trace.jsonl");
  return {
    root,
    cwd,
    famulusHome,
    tracePath,
    env: { PI_FAMULUS_HOME: famulusHome, PI_FAMULUS_MANAGER_PATH: mgr, TMPDIR: tmp },
    cleanup() {
      // The manager exits by itself 5s after its last client; do not wait.
      spawnSync(mgr, ["--home", famulusHome, "shutdown"], { stdio: "ignore", timeout: 5000 });
      // The manager may still be writing its logs (ENOTEMPTY); retry briefly,
      // then leave the temp dir rather than crash a graded episode.
      if (!opts.keep && !process.env.PI_FAMULUS_EVAL_KEEP) {
        try {
          rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
        } catch (err) {
          console.error(`sandbox cleanup: left ${root}: ${(err as Error).message}`);
        }
      }
    },
  };
}
