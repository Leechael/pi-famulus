/**
 * /reload must not end the session's background work: the reloaded extension
 * instance adopts the runtime (manager connection, subagents, monitors) the
 * old one parked, while quit and session replacement still tear it down.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  createPiBinding,
  HANDOFF_EXPIRY_MS,
  parkRuntime,
  takeParkedRuntime,
} from "../../src/reload-handoff";

const managers = vi.hoisted(() => [] as Array<{
  shutdownSession: ReturnType<typeof vi.fn>;
  close: ReturnType<typeof vi.fn>;
  emit: (event: Record<string, unknown>) => void;
}>);

const sessionControl = vi.hoisted(() => ({
  useFake: false,
  sessions: [] as Array<{ complete(text?: string): void }>,
}));

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  class FakeManagerClient {
    shutdownSession = vi.fn(async () => []);
    close = vi.fn(async () => {});
    private readonly events: Array<(event: Record<string, unknown>) => void> = [];
    constructor() {
      managers.push(this);
    }
    onEvent(handler: (event: Record<string, unknown>) => void) {
      this.events.push(handler);
    }
    onReconnect() {}
    emit(event: Record<string, unknown>) {
      for (const handler of this.events) handler(event);
    }
    async connect() { return true; }
    async ensureAvailable() { return true; }
    isAvailable() { return true; }
    protocolLevel() { return 0; }
    lastError() { return ""; }
    async list() { return []; }
    async start() { return { task_id: "sh_late", pid: 1 }; }
    async markBackground() {}
    async wait() { return { done: false, exit_code: null }; }
    async output() {
      return { chunk: "", next_cursor: 0, status: "running", exit_code: null, total_size: 0 };
    }
    async stop() {}
  }
  return { ...actual, ManagerClient: FakeManagerClient };
});

vi.mock("../../src/subagent/pi-runtime", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/subagent/pi-runtime")>();
  return {
    ...actual,
    createPiSessionFn: (deps: Parameters<typeof actual.createPiSessionFn>[0]) => {
      if (!sessionControl.useFake) return actual.createPiSessionFn(deps);
      return async () => {
        const { FakeChildSession } = await import("./subagent-fakes");
        const session = new FakeChildSession();
        session.autoComplete = null;
        sessionControl.sessions.push(session);
        return session;
      };
    },
  };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-reload-"));
process.env.PI_FAMULUS_HOME = home;
afterAll(() => rmSync(home, { recursive: true, force: true }));

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

function fakePi(options?: { stale?: { current: boolean } }) {
  const handlers = new Map<string, Handler[]>();
  const tools: string[] = [];
  const toolByName = new Map<string, ExecutableTool>();
  const commands = new Map<string, { handler: (args: string, ctx: unknown) => Promise<void> }>();
  const sent: unknown[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: ExecutableTool) => {
      tools.push(tool.name);
      toolByName.set(tool.name, tool);
    },
    registerCommand: (name: string, command: { handler: (args: string, ctx: unknown) => Promise<void> }) =>
      commands.set(name, command),
    registerMessageRenderer: () => {},
    sendMessage: (msg: unknown) => {
      if (options?.stale?.current) throw new Error("stale pi instance");
      sent.push(msg);
    },
  };
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    for (const handler of handlers.get(name) ?? []) await handler({ type: name, ...event }, ctx);
  };
  return { api: api as unknown as ExtensionAPI, tools, toolByName, commands, sent, emit };
}

function fakeCtx(sessionId = "sess-1", options?: { hasUI?: boolean; throwOnWidgetClear?: boolean }) {
  const notices: string[] = [];
  let widgetMounted = false;
  const ui = {
    notify: (msg: string) => notices.push(msg),
    setWidget: (_key: string, content: unknown) => {
      if (typeof content === "function") {
        (content as (tui: { requestRender(): void }, theme: { fg(color: string, text: string): string }) => unknown)(
          { requestRender() {} },
          { fg: (_color, text) => text },
        );
        widgetMounted = true;
        return;
      }
      if (content === undefined && widgetMounted && options?.throwOnWidgetClear) {
        options.throwOnWidgetClear = false;
        throw new Error("fleet widget dispose failed");
      }
    },
    get mounted() {
      return widgetMounted;
    },
  };
  return {
    notices,
    ui,
    ctx: {
      cwd: tmpdir(),
      hasUI: options?.hasUI === true,
      ui,
      isIdle: () => true,
      sessionManager: { getSessionId: () => sessionId, getSessionFile: () => undefined },
      modelRegistry: null,
      scopedModels: [],
      isProjectTrusted: () => false,
    },
  };
}

async function loadExtension(pi: ExtensionAPI): Promise<void> {
  vi.resetModules(); // pi re-imports extensions on /reload (jiti moduleCache: false)
  const mod = await import("../../src/index");
  mod.default(pi);
}

describe("extension across /reload", () => {
  beforeEach(() => {
    managers.length = 0;
    sessionControl.useFake = false;
    sessionControl.sessions.length = 0;
    takeParkedRuntime(); // nothing left over from another test
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps the manager session and hands the runtime to the reloaded instance", async () => {
    const before = fakePi();
    await loadExtension(before.api);
    await before.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    expect(managers).toHaveLength(1);

    await before.emit("session_shutdown", { reason: "reload" });
    const after = fakePi();
    await loadExtension(after.api);
    await after.emit("session_start", { reason: "reload" }, fakeCtx().ctx);

    expect(managers).toHaveLength(1);
    expect(managers[0].shutdownSession).not.toHaveBeenCalled();
    expect(managers[0].close).not.toHaveBeenCalled();
    expect(after.tools).toEqual(expect.arrayContaining(["bash", "subagent", "monitor", "task_list"]));
    expect([...after.commands.keys()].sort()).toEqual(["attach", "famulus", "reply", "tasks"]);

    await after.emit("session_shutdown", { reason: "quit" });
    expect(managers[0].shutdownSession).toHaveBeenCalledTimes(1);
    expect(managers[0].close).toHaveBeenCalledTimes(1);
  });

  it("still ends the session's tasks on quit", async () => {
    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await pi.emit("session_shutdown", { reason: "quit" });
    expect(managers[0].shutdownSession).toHaveBeenCalledTimes(1);
    expect(takeParkedRuntime()).toBeNull();
  });

  it("tears the parked runtime down when no instance adopts it", async () => {
    vi.useFakeTimers();
    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await pi.emit("session_shutdown", { reason: "reload" });
    expect(managers[0].shutdownSession).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(HANDOFF_EXPIRY_MS);
    expect(managers[0].shutdownSession).toHaveBeenCalledTimes(1);
  });

  it("tears down a runtime parked by another version before starting fresh", async () => {
    const dispose = vi.fn(async () => {});
    const attach = vi.fn();
    parkRuntime({ version: "0.0.0-other", runtime: { attach }, dispose });

    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "reload" }, fakeCtx().ctx);

    expect(attach).not.toHaveBeenCalled();
    expect(dispose).toHaveBeenCalledTimes(1);
    expect(managers).toHaveLength(1);
    expect(pi.tools).toContain("bash");
  });
});

describe("PiBinding", () => {
  it("replays registrations on each attached pi and queues messages until ready", () => {
    const binding = createPiBinding();
    const first = fakePi();
    binding.attach(first.api);
    binding.api.registerCommand("tasks", { handler: async () => {} } as never);
    binding.ready();
    binding.api.sendMessage({ n: 1 } as never);
    expect(first.sent).toEqual([{ n: 1 }]);

    binding.detach();
    binding.api.sendMessage({ n: 2 } as never);
    const second = fakePi();
    binding.attach(second.api);
    expect([...second.commands.keys()]).toEqual(["tasks"]);
    binding.api.sendMessage({ n: 3 } as never);
    expect(second.sent).toEqual([]);
    binding.ready();
    expect(second.sent).toEqual([{ n: 2 }, { n: 3 }]);
    expect(first.sent).toEqual([{ n: 1 }]);
  });

  it("does not deliver messages discarded before the next session starts", () => {
    const binding = createPiBinding();
    binding.attach(fakePi().api);
    binding.detach();
    binding.api.sendMessage({ n: 1 } as never);
    const next = fakePi();
    binding.attach(next.api);
    binding.discardQueued();
    binding.ready();
    expect(next.sent).toEqual([]);
  });
});

const SHUTDOWN_REASONS = ["new", "resume", "fork"] as const;

async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 30; i++) await Promise.resolve();
}

async function waitForChildSession(): Promise<{ complete(text?: string): void }> {
  const deadline = Date.now() + 2000;
  while (Date.now() < deadline) {
    const session = sessionControl.sessions[0];
    if (session) return session;
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error("background subagent session was not created");
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Start a background subagent and a background shell, shut the session down,
 * then deliver both results late. sendMessage throws once the pi instance is
 * stale, which is what a replaced session does.
 */
async function lateWakesAfterShutdown(
  reason: (typeof SHUTDOWN_REASONS)[number],
  throwOnWidgetClear: boolean,
): Promise<void> {
  sessionControl.useFake = true;
  sessionControl.sessions.length = 0;
  const stale = { current: false };
  const pi = fakePi({ stale });
  const widget = { hasUI: throwOnWidgetClear, throwOnWidgetClear };
  const host = fakeCtx("sess-switch", widget);
  await loadExtension(pi.api);
  await pi.emit("session_start", { reason: "startup" }, host.ctx);

  const bash = pi.toolByName.get("bash");
  const subagent = pi.toolByName.get("subagent");
  if (!bash || !subagent) throw new Error("bash and subagent tools were not registered");
  await bash.execute("bash-1", { command: "echo hi", run_in_background: true }, undefined, undefined, host.ctx);
  await subagent.execute(
    "sub-1",
    { async: true, tasks: [{ prompt: "inspect", name: "alpha" }] },
    undefined,
    undefined,
    host.ctx,
  );
  const session = await waitForChildSession();
  if (throwOnWidgetClear) expect(host.ui.mounted).toBe(true);

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
    stale.current = true;
    const sentBefore = pi.sent.length;
    // Only the batching clock is fake. setImmediate stays real so a rejected
    // floating promise is observed as unhandledRejection on the next turn.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    try {
      managers[0]?.emit({
        event: "task_exited",
        task_id: "sh_late",
        exit_code: 0,
        duration_ms: 10,
        output_path: "/tmp/sh_late.output",
        ts: 1,
      });
      session.complete("child finished");
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
      problems.push(`sent ${pi.sent.length - sentBefore} message(s) to pi after shutdown`);
    }
    if (unhandled.length > 0) {
      problems.push(`unhandledRejection: ${unhandled.map(errorText).join("; ")}`);
    }
    expect(problems).toEqual([]);
  } finally {
    process.off("unhandledRejection", onUnhandled);
    vi.useRealTimers();
    stale.current = false;
    try {
      await pi.emit("session_shutdown", { reason: "quit" });
    } catch {
      // A second shutdown clears child timers after the one-shot widget throw.
    }
  }
}

describe.each(SHUTDOWN_REASONS)("session_shutdown reason %s while background work is in flight", (reason) => {
  beforeEach(() => {
    managers.length = 0;
    sessionControl.useFake = false;
    sessionControl.sessions.length = 0;
    takeParkedRuntime();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("drops a late subagent result and a late shell exit", async () => {
    await lateWakesAfterShutdown(reason, false);
  });

  it("still drops them when teardown throws before the notification centre is disposed", async () => {
    await lateWakesAfterShutdown(reason, true);
  });
});
