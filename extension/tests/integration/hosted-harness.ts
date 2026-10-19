/**
 * The parent extension against the real pi-famulus daemon, with real hosted
 * children: the daemon spawns `pi --mode rpc` from this checkout's pi
 * package. Children use the fake model in tests/fixtures/faux-child.ts
 * unless `childArgs` says otherwise.
 *
 * Opt-in, like the other real-daemon suites: PI_FAMULUS_INTEG=1 and a built
 * binary. Tests use `describe.skipIf(!HOSTED_INTEG)`.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { famulusPaths } from "../../src/config";

export const MANAGER_BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/release", process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus");

const PI_CLI = join(__dirname, "../../node_modules/@earendil-works/pi-coding-agent/dist/cli.js");

/** Children run Unix shell fixtures (`tail -f /dev/null`); POSIX only. */
export const HOSTED_INTEG =
  process.env.PI_FAMULUS_INTEG === "1" && existsSync(MANAGER_BIN) && existsSync(PI_CLI) && process.platform !== "win32";

export const FAUX_CHILD = join(__dirname, "../fixtures/faux-child.ts");

type Handler = (event: unknown, ctx: unknown) => unknown;
export type Tool = {
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: undefined,
    ctx: unknown,
  ) => Promise<{ content?: Array<{ type: string; text?: string }>; details?: unknown }>;
};

export interface Sent {
  message: { customType?: string; content?: string; details?: unknown };
  options?: { triggerTurn?: boolean; deliverAs?: string };
}

export interface HostedHarness {
  home: string;
  cwd: string;
  sessionId: string;
  sent: Sent[];
  tool(name: string): Tool;
  ctx: unknown;
  emit(name: string, event: Record<string, unknown>): Promise<void>;
  daemonPid(): number;
  /** Model calls the children have made so far. */
  modelCalls(): number;
  /** Extension's child records for this session. */
  agentRecords(): Array<Record<string, unknown>>;
  cleanup(): Promise<void>;
}

const saved = new Map<string, string | undefined>();
function setEnv(key: string, value: string): void {
  if (!saved.has(key)) saved.set(key, process.env[key]);
  process.env[key] = value;
}
function restoreEnv(): void {
  for (const [key, value] of saved) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  saved.clear();
}

export async function startHostedHarness(opts: {
  scenario?: string;
  childArgs?: string[];
  env?: Record<string, string>;
  sessionId?: string;
}): Promise<HostedHarness> {
  // Short base: Unix socket paths are limited to ~104 bytes on macOS.
  const root = mkdtempSync(join(process.platform === "darwin" ? "/tmp" : tmpdir(), "pf-hosted-"));
  const home = join(root, "h");
  const cwd = join(root, "w");
  const agentDir = join(root, "a");
  for (const dir of [home, cwd, agentDir]) mkdirSync(dir, { recursive: true });
  const sessionId = opts.sessionId ?? "hosted-sess";
  setEnv("PI_FAMULUS_HOME", home);
  setEnv("PI_FAMULUS_MANAGER_PATH", MANAGER_BIN);
  setEnv("PI_CODING_AGENT_DIR", agentDir);
  setEnv("PI_FAMULUS_TEST_FAUX", opts.scenario ?? "hang");
  const tracePath = join(root, "faux-trace.txt");
  setEnv("PI_FAMULUS_TEST_FAUX_TRACE", tracePath);
  setEnv(
    "PI_FAMULUS_PI",
    JSON.stringify([
      process.execPath, PI_CLI, "-ne", "-ns", "-np", "-nc", "--offline",
      ...(opts.childArgs ?? ["-e", FAUX_CHILD, "--model", "faux/faux-1"]),
    ]),
  );
  for (const [key, value] of Object.entries(opts.env ?? {})) setEnv(key, value);

  const handlers = new Map<string, Handler[]>();
  const tools = new Map<string, Tool>();
  const sent: Sent[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: Tool & { name: string }) => tools.set(tool.name, tool),
    registerCommand: () => {},
    registerMessageRenderer: () => {},
    registerShortcut: () => {},
    registerFlag: () => {},
    registerProvider: () => {},
    sendMessage: (message: Sent["message"], options?: Sent["options"]) => sent.push({ message, options }),
  };
  const ctx = {
    cwd,
    hasUI: false,
    ui: { notify: () => {}, setWidget: () => {} },
    isIdle: () => true,
    sessionManager: { getSessionId: () => sessionId, getSessionFile: () => undefined },
    modelRegistry: null,
    scopedModels: [],
    isProjectTrusted: () => false,
  };
  const emit = async (name: string, event: Record<string, unknown>) => {
    for (const handler of handlers.get(name) ?? []) await handler({ type: name, ...event }, ctx);
  };
  const { default: extension } = await import("../../src/index");
  extension(api as unknown as ExtensionAPI);
  await emit("session_start", { reason: "startup" });
  const paths = famulusPaths(home);
  await waitUntil(() => existsSync(paths.pidFile), 15_000, "the daemon to start");

  const readJsonDir = (dir: string) => {
    if (!existsSync(dir)) return [];
    return readdirSync(dir)
      .filter((file) => file.endsWith(".json"))
      .map((file) => {
        try {
          return JSON.parse(readFileSync(join(dir, file), "utf8")) as Record<string, unknown>;
        } catch {
          return {};
        }
      });
  };

  return {
    home,
    cwd,
    sessionId,
    sent,
    ctx,
    emit,
    tool(name) {
      const found = tools.get(name);
      if (!found) throw new Error(`tool ${name} was not registered`);
      return found;
    },
    daemonPid: () => (JSON.parse(readFileSync(paths.pidFile, "utf8")) as { pid: number }).pid,
    modelCalls: () => (existsSync(tracePath) ? readFileSync(tracePath, "utf8").split("\n").filter(Boolean).length : 0),
    agentRecords: () => readJsonDir(join(home, "sessions", sessionId, "agents")),
    async cleanup() {
      try {
        await emit("session_shutdown", { reason: "quit" });
      } catch {
        // the test may already have broken the session
      }
      try {
        execFileSync(MANAGER_BIN, ["--home", home, "shutdown"], { stdio: "ignore", timeout: 5000 });
      } catch {
        // already gone
      }
      restoreEnv();
      rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    },
  };
}

export async function waitUntil(predicate: () => boolean | Promise<boolean>, timeoutMs: number, label: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs}ms waiting for ${label}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

/** True while `pid` exists (signal 0). */
export function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

export function textOf(result: { content?: Array<{ type: string; text?: string }> }): string {
  return (result.content ?? []).map((part) => part.text ?? "").join("\n");
}
