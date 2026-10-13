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

const managers = vi.hoisted(() => [] as Array<{ shutdownSession: ReturnType<typeof vi.fn>; close: ReturnType<typeof vi.fn> }>);

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  class FakeManagerClient {
    shutdownSession = vi.fn(async () => []);
    close = vi.fn(async () => {});
    constructor() {
      managers.push(this);
    }
    onEvent() {}
    onReconnect() {}
    async connect() { return true; }
    async ensureAvailable() { return true; }
    isAvailable() { return true; }
    protocolLevel() { return 0; }
    lastError() { return ""; }
    async list() { return []; }
  }
  return { ...actual, ManagerClient: FakeManagerClient };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-reload-"));
process.env.PI_FAMULUS_HOME = home;
afterAll(() => rmSync(home, { recursive: true, force: true }));

type Handler = (event: unknown, ctx: unknown) => unknown;

function fakePi() {
  const handlers = new Map<string, Handler[]>();
  const tools: string[] = [];
  const commands = new Map<string, { handler: (args: string, ctx: unknown) => Promise<void> }>();
  const sent: unknown[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: { name: string }) => tools.push(tool.name),
    registerCommand: (name: string, command: { handler: (args: string, ctx: unknown) => Promise<void> }) =>
      commands.set(name, command),
    registerMessageRenderer: () => {},
    sendMessage: (msg: unknown) => sent.push(msg),
  };
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    for (const handler of handlers.get(name) ?? []) await handler({ type: name, ...event }, ctx);
  };
  return { api: api as unknown as ExtensionAPI, tools, commands, sent, emit };
}

function fakeCtx(sessionId = "sess-1") {
  const notices: string[] = [];
  return {
    notices,
    ctx: {
      cwd: tmpdir(),
      hasUI: false,
      ui: { notify: (msg: string) => notices.push(msg) },
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
    expect([...after.commands.keys()].sort()).toEqual(["famulus", "reply", "tasks"]);

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
});
