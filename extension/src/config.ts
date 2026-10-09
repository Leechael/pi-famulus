/**
 * Extension configuration loading (design doc §4.9).
 *
 * Config file: <famulus-home>/config.json
 * Base directory resolution: PI_FAMULUS_HOME env > ~/.pi/agent/pi-famulus
 */
import { accessSync, constants, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, join, win32 } from "node:path";
import { nativePackageName, resolveNativeManagerPath } from "./native-manager.js";

export interface FamulusConfig {
  /** Foreground budget for bash before auto-backgrounding (ms). */
  foregroundBudgetMs: number;
  /** Foreground budget for subagent runs (ms). Reserved for M3. */
  subagentBudgetMs: number;
  /** Explicit path to the pi-famulus binary, or null for auto-resolution. */
  managerPath: string | null;
  logLevel: "debug" | "info" | "warn" | "error";
  /** Optional M3 subagent tuning section (design doc §4.6 limits). */
  subagent?: FamulusSubagentConfig;
}

/** Optional `subagent` section of config.json; every field defaults (see resolveSubagentConfig). */
export interface FamulusSubagentConfig {
  /** Sync-wait budget before a run is moved to background (ms). Overrides top-level subagentBudgetMs. */
  budgetMs?: number;
  /**
   * Default per-child soft budget per turn (ms). Reaching it wakes the parent
   * (subagent-overrun); the child keeps running.
   */
  timeoutMs?: number;
  /** Repeat interval of the overrun wake while a child stays past its budget (ms). Default 10 min. */
  overrunRepeatMs?: number;
  /** Opt-in ceiling per turn that aborts the child (ms). 0 or absent = off (default). */
  hardTimeoutMs?: number;
  /** Stall watchdog: abort a child with no events for this long (ms). Paused during tools and need_decision. */
  stallMs?: number;
  /** Auto-resume attempts after a stall before the run settles failed (stalled). Default 1. */
  stallRetries?: number;
  /** Pause between the stall abort and the retry prompt (ms). Default 5000. */
  stallRetryDelayMs?: number;
  /** need_decision wait for the parent (ms). Independent of stallMs and timeoutMs. */
  decisionTimeoutMs?: number;
  /** Default per-run worker pool concurrency. */
  concurrency?: number;
  /** Global cap on concurrently running children across all runs. */
  maxConcurrentChildren?: number;
  /** Max child sessions spawned per hour. */
  spawnBudgetPerHour?: number;
}

/** Subagent settings with every field resolved (design doc §4.6 defaults). */
export interface ResolvedSubagentConfig {
  budgetMs: number;
  timeoutMs: number;
  overrunRepeatMs: number;
  /** 0 = no hard ceiling. */
  hardTimeoutMs: number;
  stallMs: number;
  stallRetries: number;
  stallRetryDelayMs: number;
  decisionTimeoutMs: number;
  concurrency: number;
  maxConcurrentChildren: number;
  spawnBudgetPerHour: number;
}

/**
 * Largest delay (ms) Node's setTimeout accepts without overflow. Larger
 * configured values would clamp to ~1ms and fire immediately — cap instead.
 */
export const MAX_TIMER_DELAY_MS = 2_147_483_647;

function capTimerDelay(value: number): number {
  return Math.min(Math.floor(value), MAX_TIMER_DELAY_MS);
}

export const DEFAULT_SUBAGENT_CONFIG: ResolvedSubagentConfig = {
  budgetMs: 45000,
  timeoutMs: 1_800_000,
  overrunRepeatMs: 600_000,
  hardTimeoutMs: 0,
  stallMs: 300_000,
  stallRetries: 1,
  stallRetryDelayMs: 5_000,
  decisionTimeoutMs: 600_000,
  concurrency: 4,
  maxConcurrentChildren: 8,
  spawnBudgetPerHour: 32,
};

/** Merge defaults <- top-level subagentBudgetMs <- subagent section. */
export function resolveSubagentConfig(config: FamulusConfig): ResolvedSubagentConfig {
  const section = config.subagent ?? {};
  const resolved = { ...DEFAULT_SUBAGENT_CONFIG };
  resolved.budgetMs = config.subagentBudgetMs > 0 ? config.subagentBudgetMs : resolved.budgetMs;
  if (typeof section.budgetMs === "number" && section.budgetMs > 0) resolved.budgetMs = section.budgetMs;
  if (typeof section.timeoutMs === "number" && section.timeoutMs > 0) resolved.timeoutMs = section.timeoutMs;
  if (typeof section.overrunRepeatMs === "number" && section.overrunRepeatMs > 0) {
    resolved.overrunRepeatMs = capTimerDelay(section.overrunRepeatMs);
  }
  if (typeof section.hardTimeoutMs === "number" && section.hardTimeoutMs >= 0) {
    resolved.hardTimeoutMs = capTimerDelay(section.hardTimeoutMs);
  }
  if (typeof section.stallMs === "number" && section.stallMs > 0) {
    resolved.stallMs = capTimerDelay(section.stallMs);
  }
  if (typeof section.stallRetries === "number" && section.stallRetries >= 0) {
    resolved.stallRetries = Math.floor(section.stallRetries);
  }
  if (typeof section.stallRetryDelayMs === "number" && section.stallRetryDelayMs >= 0) {
    resolved.stallRetryDelayMs = capTimerDelay(section.stallRetryDelayMs);
  }
  if (typeof section.decisionTimeoutMs === "number" && section.decisionTimeoutMs > 0) {
    resolved.decisionTimeoutMs = section.decisionTimeoutMs;
  }
  if (typeof section.concurrency === "number" && section.concurrency >= 1) {
    resolved.concurrency = Math.floor(section.concurrency);
  }
  if (typeof section.maxConcurrentChildren === "number" && section.maxConcurrentChildren >= 1) {
    resolved.maxConcurrentChildren = Math.floor(section.maxConcurrentChildren);
  }
  if (typeof section.spawnBudgetPerHour === "number" && section.spawnBudgetPerHour >= 1) {
    resolved.spawnBudgetPerHour = Math.floor(section.spawnBudgetPerHour);
  }
  return resolved;
}

export const DEFAULT_CONFIG: FamulusConfig = {
  foregroundBudgetMs: 20000,
  subagentBudgetMs: 45000,
  managerPath: null,
  logLevel: "info",
};

/** Resolve the pi-famulus base directory. PI_FAMULUS_HOME overrides the default. */
export function getFamulusHome(env: NodeJS.ProcessEnv = process.env): string {
  const override = env.PI_FAMULUS_HOME;
  if (override && override.trim().length > 0) return override;
  return join(homedir(), ".pi", "agent", "pi-famulus");
}

/** FNV-1a 64-bit over UTF-8 bytes — must match manager `sys::fnv1a64`. */
export function fnv1a64(input: string): string {
  let h = 0xcbf29ce484222325n;
  for (const byte of Buffer.from(input, "utf8")) {
    h ^= BigInt(byte);
    h = (h * 0x0100000001b3n) & 0xffffffffffffffffn;
  }
  return h.toString(16);
}

/**
 * Named-pipe identity of a home — must match manager `lifecycle::windows_pipe_ident`.
 * The home is made absolute lexically (`path.win32.resolve` against `cwd`):
 * `.` / `..` and repeated separators collapse, symlinks and junctions are
 * not expanded. One spelling per directory (backslashes, no trailing
 * separator except a drive root, lower case). The same relative home from
 * two working directories must not share a pipe.
 */
export function windowsHomeKey(home: string, cwd: string = process.cwd()): string {
  let s = win32.resolve(cwd, home);
  while (s.length > 3 && s.endsWith("\\")) s = s.slice(0, -1);
  return s.toLowerCase();
}

export function windowsPipeName(home: string, cwd: string = process.cwd()): string {
  return `\\\\.\\pipe\\pi-famulus-${fnv1a64(windowsHomeKey(home, cwd))}`;
}

/** Well-known paths inside the pi-famulus home directory (design doc §3.1). */
export function famulusPaths(home: string, platform: NodeJS.Platform = process.platform) {
  const socket =
    platform === "win32"
      ? windowsPipeName(home)
      : join(home, "manager.sock");
  return {
    home,
    socket,
    pidFile: join(home, "manager.pid"),
    spawnLock: join(home, "manager.spawn.lock"),
    log: join(home, "manager.log"),
    config: join(home, "config.json"),
    sessionsDir: join(home, "sessions"),
  };
}

/** Full-output file path for a task (design doc §3.1 layout). */
export function taskOutputPath(home: string, sessionId: string, taskId: string): string {
  return join(home, "sessions", sessionId, "tasks", `${taskId}.output`);
}

/** Load config.json, tolerating missing/malformed files and unknown fields. */
export function loadConfig(home: string = getFamulusHome()): FamulusConfig {
  const path = famulusPaths(home).config;
  let raw: unknown;
  try {
    raw = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return { ...DEFAULT_CONFIG };
  }
  if (typeof raw !== "object" || raw === null) return { ...DEFAULT_CONFIG };
  const obj = raw as Record<string, unknown>;
  const config = { ...DEFAULT_CONFIG };
  if (typeof obj.foregroundBudgetMs === "number" && obj.foregroundBudgetMs > 0) {
    config.foregroundBudgetMs = obj.foregroundBudgetMs;
  }
  if (typeof obj.subagentBudgetMs === "number" && obj.subagentBudgetMs > 0) {
    config.subagentBudgetMs = obj.subagentBudgetMs;
  }
  if (typeof obj.managerPath === "string" && obj.managerPath.length > 0) {
    config.managerPath = obj.managerPath;
  }
  if (
    obj.logLevel === "debug" ||
    obj.logLevel === "info" ||
    obj.logLevel === "warn" ||
    obj.logLevel === "error"
  ) {
    config.logLevel = obj.logLevel;
  }
  if (typeof obj.subagent === "object" && obj.subagent !== null) {
    const section = obj.subagent as Record<string, unknown>;
    const subagent: FamulusSubagentConfig = {};
    if (typeof section.budgetMs === "number" && section.budgetMs > 0) subagent.budgetMs = section.budgetMs;
    if (typeof section.timeoutMs === "number" && section.timeoutMs > 0) subagent.timeoutMs = section.timeoutMs;
    if (typeof section.overrunRepeatMs === "number" && section.overrunRepeatMs > 0) {
      subagent.overrunRepeatMs = capTimerDelay(section.overrunRepeatMs);
    }
    if (typeof section.hardTimeoutMs === "number" && section.hardTimeoutMs >= 0) {
      subagent.hardTimeoutMs = capTimerDelay(section.hardTimeoutMs);
    }
    if (typeof section.stallMs === "number" && section.stallMs > 0) {
      subagent.stallMs = capTimerDelay(section.stallMs);
    }
    if (typeof section.stallRetries === "number" && section.stallRetries >= 0) {
      subagent.stallRetries = Math.floor(section.stallRetries);
    }
    if (typeof section.stallRetryDelayMs === "number" && section.stallRetryDelayMs >= 0) {
      subagent.stallRetryDelayMs = capTimerDelay(section.stallRetryDelayMs);
    }
    if (typeof section.decisionTimeoutMs === "number" && section.decisionTimeoutMs > 0) {
      subagent.decisionTimeoutMs = section.decisionTimeoutMs;
    }
    if (typeof section.concurrency === "number" && section.concurrency >= 1) {
      subagent.concurrency = section.concurrency;
    }
    if (typeof section.maxConcurrentChildren === "number" && section.maxConcurrentChildren >= 1) {
      subagent.maxConcurrentChildren = section.maxConcurrentChildren;
    }
    if (typeof section.spawnBudgetPerHour === "number" && section.spawnBudgetPerHour >= 1) {
      subagent.spawnBudgetPerHour = section.spawnBudgetPerHour;
    }
    config.subagent = subagent;
  }
  return config;
}

/** File name of the manager in <home>/bin and on PATH: Windows only starts `.exe` images. */
export const MANAGER_FILE_NAME = process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus";

/** Return why a candidate is unusable, or null for an executable regular file. */
function managerCandidateProblem(path: string): string | null {
  try {
    if (!statSync(path).isFile()) return "not a regular file";
    accessSync(path, constants.X_OK);
    return null;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "ENOENT" || code === "ENOTDIR") return "missing";
    if (code === "EACCES" || code === "EPERM") return "not executable";
    return "inaccessible";
  }
}

/**
 * Human-readable account of the manager search locations for the degraded-startup
 * warning, including why explicit and home/bin candidates are ignored.
 */
export function describeManagerSearch(
  config: FamulusConfig,
  home: string = getFamulusHome(),
  env: NodeJS.ProcessEnv = process.env,
): string {
  const describeCandidate = (path: string): string => {
    const problem = managerCandidateProblem(path);
    return `${path}${problem ? ` (${problem}, ignored)` : ""}`;
  };
  const tried: string[] = [];
  if (config.managerPath) tried.push(`config managerPath ${describeCandidate(config.managerPath)}`);
  if (env.PI_FAMULUS_MANAGER_PATH) tried.push(`PI_FAMULUS_MANAGER_PATH ${describeCandidate(env.PI_FAMULUS_MANAGER_PATH)}`);
  const nativeName = nativePackageName(process.platform, process.arch);
  const native = resolveNativeManagerPath();
  tried.push(nativeName
    ? `npm ${nativeName}${native ? ` ${native}` : " (missing or unusable, ignored)"}`
    : `npm native manager (unsupported ${process.platform}/${process.arch})`);
  tried.push(describeCandidate(join(home, "bin", MANAGER_FILE_NAME)));
  tried.push("pi-famulus on PATH");
  return tried.join("; ");
}

/**
 * Resolve an executable regular pi-famulus binary, or null if none is usable.
 * Priority: config.managerPath > PI_FAMULUS_MANAGER_PATH env > npm native package > <home>/bin/pi-famulus > PATH
 * (`pi-famulus.exe` on Windows).
 */
export function resolveManagerPath(
  config: FamulusConfig,
  home: string = getFamulusHome(),
  env: NodeJS.ProcessEnv = process.env,
): string | null {
  if (config.managerPath && managerCandidateProblem(config.managerPath) === null) return config.managerPath;
  const envPath = env.PI_FAMULUS_MANAGER_PATH;
  if (envPath && managerCandidateProblem(envPath) === null) return envPath;
  const native = resolveNativeManagerPath();
  if (native) return native;
  const bundled = join(home, "bin", MANAGER_FILE_NAME);
  if (managerCandidateProblem(bundled) === null) return bundled;
  for (const dir of (env.PATH ?? "").split(delimiter)) {
    if (!dir) continue;
    const candidate = join(dir, MANAGER_FILE_NAME);
    if (managerCandidateProblem(candidate) === null) return candidate;
  }
  return null;
}
