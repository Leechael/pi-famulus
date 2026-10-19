/**
 * Killing the daemon mid-task must wake the agent exactly once, with status
 * orphaned and end reason manager-crash.
 *
 * Same opt-in as the other real-manager suite (a built pi-famulus binary):
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/real-manager-orphan-wake.test.ts
 *
 * Skips when the binary is missing, so a checkout without a build does not fail.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { famulusPaths } from "../../src/config";

const RUN = process.env.PI_FAMULUS_INTEG === "1";
const BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/release", process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus");

function bashAvailable(): boolean {
  if (process.platform !== "win32") return true;
  try {
    execFileSync("where.exe", ["bash.exe"], { stdio: "ignore", windowsHide: true });
    return true;
  } catch {
    return false;
  }
}

const HAS_BIN = existsSync(BIN);
const HAS_BASH = bashAvailable();

type Handler = (event: unknown, ctx: unknown) => unknown;
type ExecutableTool = {
  name: string;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: undefined,
    ctx: unknown,
  ) => Promise<{ content?: Array<{ type: string; text?: string }> }>;
};

interface TaskWakeMessage {
  content: string;
  details: {
    kind?: string;
    tasks?: Array<{ status?: string; endReason?: string; id?: string }>;
  };
}

function isTaskWake(message: unknown): message is TaskWakeMessage {
  if (!message || typeof message !== "object") return false;
  const details = (message as { details?: { kind?: string } }).details;
  return details?.kind === "task";
}

describe.skipIf(!RUN || !HAS_BIN || !HAS_BASH)("daemon SIGKILL mid-task", () => {
  const previousHome = process.env.PI_FAMULUS_HOME;
  const previousBin = process.env.PI_FAMULUS_MANAGER_PATH;
  let home = "";
  let restore: (() => Promise<void>) | null = null;

  afterEach(async () => {
    await restore?.();
    restore = null;
    if (previousHome === undefined) delete process.env.PI_FAMULUS_HOME;
    else process.env.PI_FAMULUS_HOME = previousHome;
    if (previousBin === undefined) delete process.env.PI_FAMULUS_MANAGER_PATH;
    else process.env.PI_FAMULUS_MANAGER_PATH = previousBin;
    if (home) rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    home = "";
  });

  it.fails("sends exactly one orphaned wake with end reason manager-crash", async () => {
    home = mkdtempSync(join(tmpdir(), "pi-famulus-orphan-"));
    process.env.PI_FAMULUS_HOME = home;
    process.env.PI_FAMULUS_MANAGER_PATH = BIN;
    const paths = famulusPaths(home);
    const sent: unknown[] = [];
    let wakeCount = 0;
    let resolveWake: (() => void) | undefined;
    const firstWake = new Promise<void>((resolve) => {
      resolveWake = resolve;
    });
    const handlers = new Map<string, Handler[]>();
    const toolByName = new Map<string, ExecutableTool>();
    const api = {
      events: { emit: () => {}, on: () => () => {} },
      on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
      registerTool: (tool: ExecutableTool) => toolByName.set(tool.name, tool),
      registerCommand: () => {},
      registerMessageRenderer: () => {},
      registerShortcut: () => {},
      registerFlag: () => {},
      registerProvider: () => {},
      sendMessage: (message: unknown) => {
        sent.push(message);
        if (isTaskWake(message)) {
          wakeCount += 1;
          resolveWake?.();
        }
      },
    };
    const ctx = {
      cwd: tmpdir(),
      hasUI: false,
      ui: { notify: () => {} },
      isIdle: () => true,
      sessionManager: {
        getSessionId: () => "orphan-sess",
        getSessionFile: () => undefined,
      },
      modelRegistry: null,
      scopedModels: [],
      isProjectTrusted: () => false,
    };
    const { default: extension } = await import("../../src/index");
    extension(api as unknown as ExtensionAPI);
    restore = async () => {
      for (const handler of handlers.get("session_shutdown") ?? []) {
        await handler({ type: "session_shutdown", reason: "quit" }, ctx);
      }
      try {
        execFileSync(BIN, ["--home", home, "shutdown"], { stdio: "ignore", timeout: 5000 });
      } catch {
        // already gone
      }
    };

    for (const handler of handlers.get("session_start") ?? []) {
      await handler({ type: "session_start", reason: "startup" }, ctx);
    }
    const bash = toolByName.get("bash");
    const taskList = toolByName.get("task_list");
    if (!bash || !taskList) throw new Error("bash and task_list were not registered");

    // A bare `sleep` is rejected before it ever reaches the daemon. This
    // stays running until the daemon dies, without matching that check.
    const started = await bash.execute(
      "bash-1",
      { command: "tail -f /dev/null", run_in_background: true },
      undefined,
      undefined,
      ctx,
    );
    const startedText = (started.content ?? []).map((part) => part.text ?? "").join("\n");
    expect(startedText).toMatch(/task_id: sh_/);

    const listed = await waitFor(async () => {
      const result = await taskList.execute("list-1", {}, undefined, undefined, ctx);
      const text = (result.content ?? []).map((part) => part.text ?? "").join("\n");
      return /sh_\S+ \[shell\] running/.test(text);
    }, 10000, "background shell to show as running");
    expect(listed).toBe(true);

    const pid = (JSON.parse(readFileSync(paths.pidFile, "utf8")) as { pid: number }).pid;
    process.kill(pid, "SIGKILL");

    await Promise.race([
      firstWake,
      new Promise<never>((_, reject) => {
        setTimeout(() => reject(new Error("no task wake after the daemon was killed")), 20000);
      }),
    ]);
    // The exit wake is batched. Stay long enough for a second delivery to arrive.
    await new Promise((resolve) => setTimeout(resolve, 1000));

    const taskWakes = sent.filter(isTaskWake);
    expect(taskWakes).toHaveLength(1);
    expect(wakeCount).toBe(1);
    const task = taskWakes[0]?.details.tasks?.[0];
    expect({
      status: task?.status ?? null,
      endReason: task?.endReason ?? null,
      contentHasReason: taskWakes[0]?.content.includes("manager-crash") ?? false,
    }).toEqual({
      status: "orphaned",
      endReason: "manager-crash",
      contentHasReason: true,
    });
  }, 30000);
});

async function waitFor(predicate: () => Promise<boolean>, timeoutMs: number, label: string): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    try {
      if (await predicate()) return true;
    } catch (error) {
      last = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${label}${last ? `: ${last}` : ""}`);
}
