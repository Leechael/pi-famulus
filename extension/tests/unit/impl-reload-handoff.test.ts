/**
 * /reload must not end the session's background work: the reloaded extension
 * instance adopts the runtime (manager connection, subagents, monitors) the
 * old one parked, while quit and session replacement still tear it down.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionManager, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  createPiBinding,
  HANDOFF_EXPIRY_MS,
  parkRuntime,
  takeParkedRuntime,
} from "../../src/reload-handoff";
import { hostedFakeState, resetHostedFakes, until } from "./hosted-fakes";
// Transform the extension at collection time; tests re-evaluate it after vi.resetModules.
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const managers = hostedFakeState().instances;

const home = mkdtempSync(join(tmpdir(), "pi-famulus-reload-"));
process.env.PI_FAMULUS_HOME = home;
// The daemon is fake, so the hosted child is never started; any argv will do.
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
// Each test evaluates the whole extension graph one to three times; on a
// loaded runner one evaluation takes seconds, so 5s is not a liveness bound.
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

function fakePi(options?: { stale?: { current: boolean } }) {
  const handlers = new Map<string, Handler[]>();
  const tools: string[] = [];
  const toolByName = new Map<string, ExecutableTool>();
  const commands = new Map<string, { handler: (args: string, ctx: unknown) => Promise<void> }>();
  const sent: unknown[] = [];
  const sentWithOptions: Array<{ message: unknown; options: unknown }> = [];
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
    sendMessage: (msg: unknown, sendOpts?: unknown) => {
      if (options?.stale?.current) throw new Error("stale pi instance");
      sent.push(msg);
      sentWithOptions.push({ message: msg, options: sendOpts });
    },
  };
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    for (const handler of handlers.get(name) ?? []) await handler({ type: name, ...event }, ctx);
  };
  return { api: api as unknown as ExtensionAPI, tools, toolByName, commands, sent, sentWithOptions, emit };
}

function fakeCtx(sessionId = "sess-1", options?: { hasUI?: boolean; throwOnWidgetClear?: boolean; sessionFile?: string }) {
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
      sessionManager: { getSessionId: () => sessionId, getSessionFile: () => options?.sessionFile },
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
    resetHostedFakes();
    takeParkedRuntime(); // nothing left over from another test
    rmSync(join(home, "config.json"), { force: true });
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
    expect(managers[0].shutdownCalls).toBe(0);
    expect(managers[0].closeCalls).toBe(0);
    expect(after.tools).toEqual(expect.arrayContaining(["bash", "subagent", "monitor", "task_list"]));
    expect([...after.commands.keys()].sort()).toEqual(["attach", "famulus", "reply", "tasks"]);

    await after.emit("session_shutdown", { reason: "quit" });
    expect(managers[0].shutdownCalls).toBe(1);
    expect(managers[0].closeCalls).toBe(1);
  });

  it("applies messaging opt-in on reload without replacing the existing manager runtime", async () => {
    const before = fakePi();
    await loadExtension(before.api);
    expect(before.tools).not.toContain("SendMessage");
    expect(before.commands.has("famulus")).toBe(true);
    await before.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    const manager = managers[0];
    const messagingOps = vi.fn(async () => ({ session_id: "pi/sess-1" }));
    Object.assign(manager, {
      intercom: messagingOps,
      status: async () => ({ sessions: [], messaging: { state: "local_only", name: "test" } }),
    });
    const eventListeners = new Set<Function>();
    const reconnectListeners = new Set<Function>();
    const onEvent = manager.onEvent.bind(manager);
    const offEventSpy = vi.spyOn(manager, "onEvent").mockImplementation((handler) => {
      eventListeners.add(handler);
      const off = onEvent(handler);
      return () => { eventListeners.delete(handler); off(); };
    });
    const offReconnectSpy = vi.spyOn(manager, "onReconnect").mockImplementation((handler) => {
      reconnectListeners.add(handler);
      return () => { reconnectListeners.delete(handler); };
    });
    const retryTimers = new Set<ReturnType<typeof setInterval>>();
    const setIntervalOriginal = globalThis.setInterval;
    const clearIntervalOriginal = globalThis.clearInterval;
    const intervalSpy = vi.spyOn(globalThis, "setInterval").mockImplementation(((callback: any, ms: number, ...args: any[]) => {
      const timer = setIntervalOriginal(callback, ms, ...args);
      if (ms === 3000) retryTimers.add(timer);
      return timer;
    }) as typeof setInterval);
    const clearSpy = vi.spyOn(globalThis, "clearInterval").mockImplementation((timer) => {
      retryTimers.delete(timer as ReturnType<typeof setInterval>);
      clearIntervalOriginal(timer);
    });
    expect(retryTimers.size).toBe(0);
    expect(eventListeners.size).toBe(0);
    await before.emit("session_shutdown", { reason: "reload" });
    writeFileSync(join(home, "config.json"), JSON.stringify({ messaging: { enabled: true } }));
    const enabled = fakePi();
    await loadExtension(enabled.api);
    expect(enabled.tools).toContain("SendMessage");
    expect(enabled.commands.has("rename")).toBe(true);
    expect(enabled.commands.has("intercom")).toBe(false);
    await enabled.emit("session_start", { reason: "reload" }, fakeCtx().ctx);
    expect(retryTimers.size).toBe(1);
    expect(eventListeners.size).toBe(2);
    expect(reconnectListeners.size).toBe(1);
    expect(messagingOps).toHaveBeenCalledWith(expect.objectContaining({ op: "register" }));
    await enabled.emit("session_shutdown", { reason: "reload" });
    expect(retryTimers.size).toBe(0);
    expect(eventListeners.size).toBe(0);
    expect(reconnectListeners.size).toBe(0);
    writeFileSync(join(home, "config.json"), JSON.stringify({ messaging: { enabled: false } }));
    const disabled = fakePi();
    await loadExtension(disabled.api);
    expect(disabled.tools).not.toContain("SendMessage");
    expect(disabled.commands.has("rename")).toBe(false);
    expect(disabled.commands.has("intercom")).toBe(false);
    expect(disabled.commands.has("famulus")).toBe(true);
    await disabled.emit("session_start", { reason: "reload" }, fakeCtx().ctx);
    expect(managers).toHaveLength(1);
    expect(managers[0].closeCalls).toBe(0);
    expect(retryTimers.size).toBe(0);
    expect(eventListeners.size).toBe(0);
    expect(reconnectListeners.size).toBe(0);
    expect(messagingOps.mock.calls).toHaveLength(2); // register, unregister only
    await disabled.emit("session_shutdown", { reason: "quit" });
    offEventSpy.mockRestore();
    offReconnectSpy.mockRestore();
    intervalSpy.mockRestore();
    clearSpy.mockRestore();
  });

  it("does not replay unknown finished shells after a same-version reload", async () => {
    const before = fakePi();
    await loadExtension(before.api);
    await before.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await flushMicrotasks();
    hostedFakeState().tasks = ["bash-fg", "bash-bg"].map((via) => ({
      task_id: `sh_history_${via}`,
      session_id: "sess-1",
      kind: "shell",
      command: "printf history",
      origin: { via },
      status: "completed",
      exit_code: 0,
      started_at: Date.now() - 1000,
      ended_at: Date.now() - 500,
    }));
    await before.emit("session_shutdown", { reason: "reload" });
    const after = fakePi();
    await loadExtension(after.api);
    const host = fakeCtx();
    await after.emit("session_start", { reason: "reload" }, host.ctx);
    try {
      await after.toolByName.get("task_list")!.execute("list", { all: true }, undefined, undefined, host.ctx);
      await new Promise((resolve) => setTimeout(resolve, 300));
      expect(managers).toHaveLength(1);
      expect(before.sent).toHaveLength(0);
      expect(after.sent).toHaveLength(0);
    } finally {
      await after.emit("session_shutdown", { reason: "quit" });
    }
  });

  it("still ends the session's tasks on quit", async () => {
    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await pi.emit("session_shutdown", { reason: "quit" });
    expect(managers[0].shutdownCalls).toBe(1);
    expect(takeParkedRuntime()).toBeNull();
  });

  it("tears the parked runtime down when no instance adopts it", async () => {
    vi.useFakeTimers();
    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await pi.emit("session_shutdown", { reason: "reload" });
    expect(managers[0].shutdownCalls).toBe(0);

    await vi.advanceTimersByTimeAsync(HANDOFF_EXPIRY_MS);
    expect(managers[0].shutdownCalls).toBe(1);
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
  for (let i = 0; i < 30; i++) {
    await Promise.resolve();
  }
}

/**
 * The hosted child has been spawned and prompted; returns its child id. Its
 * session is the production HostedRpcSession, which settles when the daemon
 * reports that the child's run settled.
 */
async function waitForHostedChild(): Promise<string> {
  await until(
    () => (managers[0]?.inputs ?? []).some((input) => input.op === "prompt"),
    "the background subagent to be spawned and prompted",
  );
  return managers[0].spawns[0].childId;
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
  const childId = await waitForHostedChild();
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
      managers[0]?.finish(childId, { content: [{ type: "text", text: "child finished" }], stopReason: "stop" });
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
    resetHostedFakes();
    takeParkedRuntime();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("drops a late subagent result and a late shell exit", async () => {
    await lateWakesAfterShutdown(reason, false);
  });

  it.fails("still drops them when teardown throws before the notification centre is disposed", async () => {
    await lateWakesAfterShutdown(reason, true);
  });
});

function persistSessionFile(label: string): { dir: string; file: string } {
  const dir = mkdtempSync(join(tmpdir(), `famulus-old-sess-${label}-`));
  const sm = SessionManager.create(dir, join(dir, "sessions"));
  sm.appendMessage({ role: "user", content: `seed ${label}`, timestamp: Date.now() });
  const file = sm.getSessionFile();
  if (!file) throw new Error("session file was not created");
  return { dir, file };
}

function customMessages(file: string | undefined): Array<Record<string, unknown>> {
  if (!file || !existsSync(file)) return [];
  return readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as Record<string, unknown>)
    .filter((entry) => entry.type === "custom_message");
}

function teardownNotices(sent: unknown[]): Array<{ content: string; details: Record<string, unknown>; options?: unknown }> {
  return sent.flatMap((entry) => {
    const message = entry as { customType?: string; content?: string; details?: Record<string, unknown> };
    if (message.customType !== "pi-famulus-wake") return [];
    const details = message.details ?? {};
    if (details.kind !== "monitor-session-end") return [];
    return [{ content: message.content ?? "", details, options: undefined }];
  });
}

function noticesWithOptions(pi: { sentWithOptions: Array<{ message: unknown; options: unknown }> }) {
  return pi.sentWithOptions.flatMap((entry) => {
    const message = entry.message as { customType?: string; content?: string; details?: Record<string, unknown> };
    if (message.customType !== "pi-famulus-wake") return [];
    const details = message.details ?? {};
    if (details.kind !== "monitor-session-end") return [];
    return [{ content: message.content ?? "", details, options: entry.options }];
  });
}

async function startMonitor(
  pi: { toolByName: Map<string, ExecutableTool> },
  ctx: unknown,
  taskId: string,
  description: string,
): Promise<void> {
  const manager = managers[managers.length - 1];
  manager.start = async () => ({ task_id: taskId, pid: 1 });
  const tool = pi.toolByName.get("monitor");
  if (!tool) throw new Error("monitor tool was not registered");
  await tool.execute(
    "mon-call",
    { command: "tail -F /tmp/x", description, persistent: true },
    undefined,
    undefined,
    ctx,
  );
}

const REASON_MAP = { new: "new", resume: "resumed", fork: "forked" } as const;

describe("monitor teardown notices", () => {
  const sessionDirs: string[] = [];
  beforeEach(() => {
    resetHostedFakes();
    takeParkedRuntime();
  });
  afterEach(() => {
    vi.useRealTimers();
    while (sessionDirs.length > 0) {
      const dir = sessionDirs.pop();
      if (dir) rmSync(dir, { recursive: true, force: true });
    }
  });

  it("does not notify on a same-version reload while a monitor keeps running", async () => {
    const before = fakePi();
    await loadExtension(before.api);
    const host = fakeCtx();
    await before.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(before, host.ctx, "mon_reload", "deploy watch");
    await before.emit("session_shutdown", { reason: "reload" });
    const after = fakePi();
    await loadExtension(after.api);
    await after.emit("session_start", { reason: "reload" }, fakeCtx().ctx);
    try {
      expect({
        shutdownCalls: managers[0].shutdownCalls,
        beforeNotices: teardownNotices(before.sent),
        afterNotices: teardownNotices(after.sent),
      }).toEqual({ shutdownCalls: 0, beforeNotices: [], afterNotices: [] });
    } finally {
      await after.emit("session_shutdown", { reason: "quit" });
    }
  });

  it("does not notify on quit even with a running monitor", async () => {
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx();
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_quit", "quit watch");
    await pi.emit("session_shutdown", { reason: "quit" });
    expect(teardownNotices(pi.sent)).toEqual([]);
    expect(managers[0].shutdownCalls).toBe(1);
  });

  it("does not notify when new has no running monitor", async () => {
    const pi = fakePi();
    await loadExtension(pi.api);
    await pi.emit("session_start", { reason: "startup" }, fakeCtx().ctx);
    await pi.emit("session_shutdown", { reason: "new" });
    expect(teardownNotices(pi.sent)).toEqual([]);
    expect(pi.sent).toEqual([]);
  });
});

describe.each(SHUTDOWN_REASONS)("monitor teardown notice for shutdown reason %s", (reason) => {
  beforeEach(() => {
    resetHostedFakes();
    takeParkedRuntime();
  });

  it("sends one passive old-session message listing running monitors before shutdownSession", async () => {
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx();
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    const order: string[] = [];
    const manager = managers[0];
    const origShutdown = manager.shutdownSession.bind(manager);
    manager.shutdownSession = async () => {
      order.push("shutdownSession");
      return origShutdown();
    };
    const origSend = pi.api.sendMessage.bind(pi.api);
    (pi.api as { sendMessage: typeof origSend }).sendMessage = (msg, sendOpts) => {
      order.push("sendMessage");
      return origSend(msg, sendOpts);
    };
    await startMonitor(pi, host.ctx, "mon_alpha", "alpha logs");
    manager.start = async () => ({ task_id: "mon_beta", pid: 2 });
    await pi.toolByName.get("monitor")!.execute(
      "mon-call-2",
      { command: "tail -F /tmp/y", description: "beta logs", persistent: true },
      undefined,
      undefined,
      host.ctx,
    );
    await pi.emit("session_shutdown", { reason });
    const mapped = REASON_MAP[reason];
    const notices = noticesWithOptions(pi);
    expect({
      count: notices.length,
      reason: notices[0]?.details.reason,
      ids: notices[0]?.details.ids,
      options: notices[0]?.options,
      order,
      content: notices[0]?.content,
    }).toEqual({
      count: 1,
      reason: mapped,
      ids: ["mon_alpha", "mon_beta"],
      options: { triggerTurn: false },
      order: ["sendMessage", "shutdownSession"],
      content: notices[0]?.content,
    });
    expect(notices[0]?.content).toContain("mon_alpha");
    expect(notices[0]?.content).toContain("alpha logs");
    expect(notices[0]?.content).toContain("mon_beta");
    expect(notices[0]?.content).toContain("beta logs");
    expect(notices[0]?.content).toContain(mapped);
    expect(notices[0]?.content).toMatch(/elapsed/i);
    expect(notices[0]?.content).toMatch(/task_output/);
    expect(notices[0]?.content).toContain("while the log is retained");
  });
});

describe("monitor teardown notice for handoff expiry and session mismatch", () => {
  const sessionDirs: string[] = [];
  beforeEach(() => {
    resetHostedFakes();
    takeParkedRuntime();
  });
  afterEach(() => {
    vi.useRealTimers();
    while (sessionDirs.length > 0) {
      const dir = sessionDirs.pop();
      if (dir) rmSync(dir, { recursive: true, force: true });
    }
  });

  it("appends one handoff-ended notice to the old session file, not via the detached pi", async () => {
    vi.useFakeTimers();
    const persisted = persistSessionFile("expiry");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx("sess-1", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_expiry", "expiry watch");
    await pi.emit("session_shutdown", { reason: "reload" });
    expect(managers[0].shutdownCalls).toBe(0);
    await vi.advanceTimersByTimeAsync(HANDOFF_EXPIRY_MS);
    const fileCustoms = customMessages(persisted.file);
    expect({
      piNotices: teardownNotices(pi.sent),
      shutdownCalls: managers[0].shutdownCalls,
      fileCustomTypes: fileCustoms.map((entry) => entry.customType),
      fileContents: fileCustoms.map((entry) => entry.content),
      fileDetails: fileCustoms.map((entry) => entry.details),
    }).toEqual({
      piNotices: [],
      shutdownCalls: 1,
      fileCustomTypes: ["pi-famulus-wake"],
      fileContents: fileCustoms.map((entry) => entry.content),
      fileDetails: [
        expect.objectContaining({
          kind: "monitor-session-end",
          reason: "handoff-ended",
          ids: ["mon_expiry"],
        }),
      ],
    });
    expect(String(fileCustoms[0]?.content)).toContain("mon_expiry");
    expect(String(fileCustoms[0]?.content)).toContain("expiry watch");
    expect(String(fileCustoms[0]?.content)).toContain("handoff-ended");
    expect(String(fileCustoms[0]?.content)).toMatch(/elapsed/i);
    expect(String(fileCustoms[0]?.content)).toMatch(/task_output/);
    expect(String(fileCustoms[0]?.content)).toContain("while the log is retained");
  });

  it("writes session-mismatch to the old session file and not the new pi", async () => {
    const persisted = persistSessionFile("mismatch");
    sessionDirs.push(persisted.dir);
    const before = fakePi();
    await loadExtension(before.api);
    const host = fakeCtx("sess-old", { sessionFile: persisted.file });
    await before.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(before, host.ctx, "mon_mis", "mismatch watch");
    await before.emit("session_shutdown", { reason: "reload" });
    const after = fakePi();
    await loadExtension(after.api);
    const next = fakeCtx("sess-new");
    await after.emit("session_start", { reason: "reload" }, next.ctx);
    const fileCustoms = customMessages(persisted.file);
    try {
      expect({
        beforeNotices: teardownNotices(before.sent),
        afterNotices: teardownNotices(after.sent),
        afterSent: after.sent,
        fileCustomTypes: fileCustoms.map((entry) => entry.customType),
        fileDetails: fileCustoms.map((entry) => entry.details),
        shutdownCalls: managers[0].shutdownCalls,
      }).toEqual({
        beforeNotices: [],
        afterNotices: [],
        afterSent: [],
        fileCustomTypes: ["pi-famulus-wake"],
        fileDetails: [
          expect.objectContaining({
            kind: "monitor-session-end",
            reason: "session-mismatch",
            ids: ["mon_mis"],
          }),
        ],
        shutdownCalls: 1,
      });
      expect(String(fileCustoms[0]?.content)).toContain("mon_mis");
      expect(String(fileCustoms[0]?.content)).toContain("mismatch watch");
      expect(String(fileCustoms[0]?.content)).toContain("session-mismatch");
      expect(String(fileCustoms[0]?.content)).toMatch(/task_output/);
      expect(String(fileCustoms[0]?.content)).toContain("while the log is retained");
    } finally {
      try {
        await after.emit("session_shutdown", { reason: "quit" });
      } catch {
        // already torn down
      }
    }
  });

  it("does not write handoff-expiry to a previous session after a file-less session starts", async () => {
    vi.useFakeTimers();
    const persisted = persistSessionFile("stale-file");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const first = fakeCtx("sess-a", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, first.ctx);
    await startMonitor(pi, first.ctx, "mon_a", "first watch");
    await pi.emit("session_shutdown", { reason: "new" });
    const second = fakeCtx("sess-b");
    await pi.emit("session_start", { reason: "new" }, second.ctx);
    await startMonitor(pi, second.ctx, "mon_b", "fileless watch");
    await pi.emit("session_shutdown", { reason: "reload" });
    await vi.advanceTimersByTimeAsync(HANDOFF_EXPIRY_MS);
    const fileCustoms = customMessages(persisted.file);
    const expiryOnPrevious = fileCustoms.filter((entry) => {
      const details = entry.details as { reason?: string; ids?: string[] } | undefined;
      return details?.reason === "handoff-ended" || (details?.ids ?? []).includes("mon_b");
    });
    expect({
      expiryOnPrevious,
      contents: fileCustoms.map((entry) => entry.content),
      details: fileCustoms.map((entry) => entry.details),
    }).toEqual({
      expiryOnPrevious: [],
      contents: fileCustoms.map((entry) => entry.content),
      details: fileCustoms.map((entry) => entry.details),
    });
    expect(String(fileCustoms.map((entry) => entry.content).join("\n"))).not.toContain("mon_b");
  });

  it("falls back to the old session file when live sendMessage throws", async () => {
    const persisted = persistSessionFile("live-fail");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx("sess-1", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_fail", "fallback watch");
    (pi.api as { sendMessage: (message: unknown, options?: unknown) => void }).sendMessage = () => {
      throw new Error("stale pi instance");
    };
    await pi.emit("session_shutdown", { reason: "new" });
    const fileCustoms = customMessages(persisted.file);
    expect({
      piNotices: teardownNotices(pi.sent),
      fileCustomTypes: fileCustoms.map((entry) => entry.customType),
      fileDetails: fileCustoms.map((entry) => entry.details),
      content: fileCustoms[0]?.content,
    }).toEqual({
      piNotices: [],
      fileCustomTypes: ["pi-famulus-wake"],
      fileDetails: [
        expect.objectContaining({
          kind: "monitor-session-end",
          reason: "new",
          ids: ["mon_fail"],
        }),
      ],
      content: fileCustoms[0]?.content,
    });
    expect(String(fileCustoms[0]?.content)).toContain("mon_fail");
    expect(String(fileCustoms[0]?.content)).toContain("fallback watch");
    expect(String(fileCustoms[0]?.content)).toMatch(/task_output/);
    expect(String(fileCustoms[0]?.content)).toContain("while the log is retained");
  });

  it("falls back to the old session file when live sendMessage returns a rejected promise", async () => {
    const persisted = persistSessionFile("live-reject");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx("sess-1", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_reject", "rejected fallback watch");
    (pi.api as unknown as { sendMessage: (message: unknown, options?: unknown) => Promise<void> }).sendMessage = () =>
      Promise.reject(new Error("async stale pi instance"));
    await pi.emit("session_shutdown", { reason: "new" });
    const fileCustoms = customMessages(persisted.file);
    expect(fileCustoms.map((entry) => entry.details)).toEqual([
      expect.objectContaining({ kind: "monitor-session-end", reason: "new", ids: ["mon_reject"] }),
    ]);
    expect(String(fileCustoms[0]?.content)).toContain("rejected fallback watch");
    expect(String(fileCustoms[0]?.content)).toContain("while the log is retained");
  });

  it("parkRuntime replacing a parked runtime notices handoff-ended", async () => {
    const persisted = persistSessionFile("replaced");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx("sess-1", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_rep", "replaced watch");
    await pi.emit("session_shutdown", { reason: "reload" });
    parkRuntime({
      version: "0.0.0-other",
      runtime: { attach() {} },
      dispose: async () => {},
    });
    const fileCustoms = customMessages(persisted.file);
    expect(fileCustoms.map((entry) => entry.details)).toEqual([
      expect.objectContaining({
        kind: "monitor-session-end",
        reason: "handoff-ended",
        ids: ["mon_rep"],
      }),
    ]);
    expect(String(fileCustoms[0]?.content)).toContain("mon_rep");
    expect(teardownNotices(pi.sent)).toEqual([]);
  });

  it("version rejection of a parked runtime notices handoff-ended", async () => {
    const persisted = persistSessionFile("version");
    sessionDirs.push(persisted.dir);
    const pi = fakePi();
    await loadExtension(pi.api);
    const host = fakeCtx("sess-1", { sessionFile: persisted.file });
    await pi.emit("session_start", { reason: "startup" }, host.ctx);
    await startMonitor(pi, host.ctx, "mon_ver", "version watch");
    await pi.emit("session_shutdown", { reason: "reload" });
    const key = Symbol.for("pi-famulus.reload-handoff");
    const slot = (globalThis as Record<symbol, { parked?: { version: string } } | undefined>)[key];
    expect(slot?.parked).toBeTruthy();
    slot!.parked!.version = "0.0.0-other";
    const after = fakePi();
    await loadExtension(after.api);
    await after.emit("session_start", { reason: "reload" }, fakeCtx("sess-1").ctx);
    const fileCustoms = customMessages(persisted.file);
    try {
      expect({
        afterNotices: teardownNotices(after.sent),
        fileDetails: fileCustoms.map((entry) => entry.details),
      }).toEqual({
        afterNotices: [],
        fileDetails: [
          expect.objectContaining({
            kind: "monitor-session-end",
            reason: "handoff-ended",
            ids: ["mon_ver"],
          }),
        ],
      });
      expect(String(fileCustoms[0]?.content)).toContain("mon_ver");
    } finally {
      try {
        await after.emit("session_shutdown", { reason: "quit" }, fakeCtx().ctx);
      } catch {
        // torn down by version reject
      }
    }
  });
});
