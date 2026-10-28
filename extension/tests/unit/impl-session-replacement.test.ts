/**
 * /new, /resume and /fork replace the session on the same extension
 * instance: session_shutdown then session_start, no re-import. Background
 * work started before the switch must not deliver its wake after the
 * replacement, and must not throw as an unhandledRejection.
 *
 * Distinct from the shutdown-only cases in impl-reload-handoff.test.ts,
 * which never start the next session. The old client's handlers are
 * dropped on teardown and must not follow a later notify-centre rebinding.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { takeParkedRuntime } from "../../src/reload-handoff";
import { hostedFakeState, resetHostedFakes, until } from "./hosted-fakes";
// Transform the extension at collection time; tests re-evaluate it after vi.resetModules.
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const managers = hostedFakeState().instances;

const home = mkdtempSync(join(tmpdir(), "pi-famulus-replace-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
// Each test evaluates the whole extension graph; on a loaded runner that
// takes seconds, so 5s is not a liveness bound.
vi.setConfig({ testTimeout: 20_000 });

type Handler = (event: unknown, ctx: unknown) => unknown;

type ExecutableTool = {
  name: string;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: undefined,
    ctx: unknown,
  ) => Promise<unknown>;
};

function fakePi() {
  const handlers = new Map<string, Handler[]>();
  const toolByName = new Map<string, ExecutableTool>();
  const sent: unknown[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: ExecutableTool) => {
      toolByName.set(tool.name, tool);
    },
    registerCommand: () => {},
    registerMessageRenderer: () => {},
    sendMessage: (msg: unknown) => {
      sent.push(msg);
    },
  };
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    for (const handler of handlers.get(name) ?? []) {
      const result = handler({ type: name, ...event }, ctx);
      await result;
    }
  };
  return { api: api as unknown as ExtensionAPI, toolByName, sent, emit };
}

function fakeCtx(sessionId: string) {
  return {
    cwd: tmpdir(),
    hasUI: false,
    ui: { notify: () => {}, setWidget: () => {} },
    isIdle: () => true,
    sessionManager: { getSessionId: () => sessionId, getSessionFile: () => undefined },
    modelRegistry: null,
    scopedModels: [],
    isProjectTrusted: () => false,
  };
}

async function loadExtension(pi: ExtensionAPI): Promise<void> {
  vi.resetModules();
  const mod = await import("../../src/index");
  mod.default(pi);
}

const REPLACE_REASONS = ["new", "resume", "fork"] as const;

async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 30; i++) {
    await Promise.resolve();
  }
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Start background work, replace the session on the same extension
 * instance, then deliver both results late on the old manager.
 */
async function lateWakesAfterReplacement(reason: (typeof REPLACE_REASONS)[number]): Promise<void> {
  const pi = fakePi();
  const before = fakeCtx("sess-before");
  await loadExtension(pi.api);
  await pi.emit("session_start", { reason: "startup" }, before);

  const bash = pi.toolByName.get("bash");
  const subagent = pi.toolByName.get("subagent");
  if (!bash || !subagent) throw new Error("bash and subagent tools were not registered");
  await bash.execute("bash-1", { command: "echo hi", run_in_background: true }, undefined, undefined, before);
  await subagent.execute(
    "sub-1",
    { async: true, tasks: [{ prompt: "inspect", name: "alpha" }] },
    undefined,
    undefined,
    before,
  );
  await until(
    () => (managers[0]?.inputs ?? []).some((input) => input.op === "prompt"),
    "the background subagent to be spawned and prompted",
  );
  const oldManager = managers[0];
  const childId = oldManager.spawns[0].childId;

  const unhandled: unknown[] = [];
  const onUnhandled = (rejection: unknown) => {
    unhandled.push(rejection);
  };
  process.on("unhandledRejection", onUnhandled);
  const problems: string[] = [];
  try {
    try {
      await pi.emit("session_shutdown", { reason });
    } catch (error) {
      problems.push(`session_shutdown threw: ${errorText(error)}`);
    }
    const after = fakeCtx(`sess-${reason}`);
    try {
      await pi.emit("session_start", { reason }, after);
    } catch (error) {
      problems.push(`session_start threw: ${errorText(error)}`);
    }
    const sentBefore = pi.sent.length;
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    try {
      oldManager.emit({
        event: "task_exited",
        task_id: "sh_late",
        exit_code: 0,
        duration_ms: 10,
        output_path: "/tmp/sh_late.output",
        ts: 1,
      });
      oldManager.finish(childId, { content: [{ type: "text", text: "child finished" }], stopReason: "stop" });
      await flushMicrotasks();
      try {
        await vi.advanceTimersByTimeAsync(250);
      } catch (error) {
        problems.push(`late shell exit threw: ${errorText(error)}`);
      }
      await flushMicrotasks();
    } finally {
      vi.useRealTimers();
    }
    await new Promise((resolve) => setImmediate(resolve));
    await new Promise((resolve) => setImmediate(resolve));
    if (pi.sent.length !== sentBefore) {
      problems.push(`sent ${pi.sent.length - sentBefore} message(s) to pi after replacement`);
    }
    if (unhandled.length > 0) {
      problems.push(`unhandledRejection: ${unhandled.map(errorText).join("; ")}`);
    }
    expect(problems).toEqual([]);
  } finally {
    process.off("unhandledRejection", onUnhandled);
    vi.useRealTimers();
    try {
      await pi.emit("session_shutdown", { reason: "quit" });
    } catch {
      // best-effort cleanup
    }
  }
}

describe.each(REPLACE_REASONS)("session replacement reason %s while background work is in flight", (reason) => {
  beforeEach(() => {
    resetHostedFakes();
    takeParkedRuntime();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not deliver a late result after the next session starts", async () => {
    await lateWakesAfterReplacement(reason);
  });
});
