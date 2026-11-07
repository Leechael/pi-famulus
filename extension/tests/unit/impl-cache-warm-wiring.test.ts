/**
 * Cache warming through the real extension runtime: the hooks reach pi via
 * the reload binding, persistent monitors come from the real MonitorRegistry,
 * and config.json switches the feature.
 */
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { takeParkedRuntime } from "../../src/reload-handoff";
import { hostedFakeState, resetHostedFakes } from "./hosted-fakes";
// Transform the extension at collection time; tests re-evaluate it after vi.resetModules.
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const managers = hostedFakeState().instances;
const home = mkdtempSync(join(tmpdir(), "pi-famulus-cache-warm-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
vi.setConfig({ testTimeout: 20_000 });

type Handler = (event: unknown, ctx: unknown) => unknown;
type Tool = { name: string; execute: (...args: unknown[]) => Promise<unknown> };

function fakePi() {
  const handlers = new Map<string, Handler[]>();
  const tools = new Map<string, Tool>();
  const entries: { type: string; data: unknown }[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: Tool) => tools.set(tool.name, tool),
    registerCommand: () => {},
    registerMessageRenderer: () => {},
    sendMessage: () => {},
    appendEntry: (type: string, data: unknown) => entries.push({ type, data }),
  };
  /** Emit and collect handler results (pi uses them for decision events). */
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    const results: unknown[] = [];
    for (const handler of handlers.get(name) ?? []) results.push(await handler({ type: name, ...event }, ctx));
    return results.filter((r) => r !== undefined);
  };
  return { api: api as unknown as ExtensionAPI, tools, entries, emit };
}

function fakeCtx() {
  const notices: { text: string; level: string }[] = [];
  let idle = true;
  const ctx = {
    cwd: tmpdir(),
    hasUI: true,
    ui: { notify: (text: string, level: string) => notices.push({ text, level }), setWidget: () => {} },
    isIdle: () => idle,
    model: { provider: "anthropic", id: "claude-x", api: "anthropic-messages", promptCache: { short: 300 } },
    sessionManager: { getSessionId: () => "sess-1", getSessionFile: () => undefined },
    modelRegistry: { streamSimple: vi.fn() },
    scopedModels: [],
    isProjectTrusted: () => false,
  };
  return { ctx, notices, setIdle: (v: boolean) => { idle = v; } };
}

async function boot(config?: Record<string, unknown>) {
  if (config) writeFileSync(join(home, "config.json"), JSON.stringify(config));
  vi.resetModules();
  const mod = await import("../../src/index");
  const pi = fakePi();
  mod.default(pi.api);
  const host = fakeCtx();
  await pi.emit("session_start", { reason: "startup" }, host.ctx);
  return { pi, host };
}

async function startMonitor(pi: ReturnType<typeof fakePi>, ctx: unknown, taskId: string, params: Record<string, unknown>) {
  managers[managers.length - 1].start = async () => ({ task_id: taskId, pid: 1 });
  await pi.tools.get("monitor")!.execute(`call-${taskId}`, { command: "tail -F /tmp/x", description: taskId, ...params }, undefined, undefined, ctx);
}

async function realTurn(pi: ReturnType<typeof fakePi>, host: ReturnType<typeof fakeCtx>) {
  host.setIdle(false);
  await pi.emit("agent_start", {}, host.ctx);
  const payload = { model: "claude-x", max_tokens: 32000, messages: [{ role: "user", content: "hi" }] };
  const results = await pi.emit("before_provider_request", { payload }, host.ctx);
  host.setIdle(true);
  await pi.emit("agent_settled", { aborted: false }, host.ctx);
  return { payload, results };
}

describe("cache warming in the extension runtime", () => {
  beforeEach(() => {
    resetHostedFakes();
    takeParkedRuntime();
    rmSync(join(home, "config.json"), { force: true });
  });

  it("reminds once when the first persistent monitor starts with warming off", async () => {
    const { pi, host } = await boot();
    await startMonitor(pi, host.ctx, "mon_timed", { timeout_ms: 60_000 });
    expect(host.notices.filter((n) => n.text.includes("cache"))).toEqual([]);
    await startMonitor(pi, host.ctx, "mon_a", { persistent: true });
    await startMonitor(pi, host.ctx, "mon_b", { persistent: true });
    const reminders = host.notices.filter((n) => n.text.includes("Idle cache warming"));
    expect(reminders).toHaveLength(1);
    expect(reminders[0].level).toBe("info");
    expect(reminders[0].text).toContain(join(home, "config.json"));
    await pi.emit("session_shutdown", { reason: "quit" });
  });

  it("with the flag off, pi hooks return nothing", async () => {
    const { pi, host } = await boot();
    await startMonitor(pi, host.ctx, "mon_a", { persistent: true });
    const { results } = await realTurn(pi, host);
    expect(results).toEqual([]);
    expect(await pi.emit("cache_warming_decision", { action: "warm" }, host.ctx)).toEqual([]);
    await pi.emit("session_shutdown", { reason: "quit" });
  });

  it("with the flag on, owns idle warming only while a persistent monitor runs", async () => {
    const { pi, host } = await boot({ cacheWarming: { enabled: true } });
    // No persistent monitor: pi keeps its own decision.
    await realTurn(pi, host);
    expect(await pi.emit("cache_warming_decision", { action: "warm" }, host.ctx)).toEqual([]);
    await startMonitor(pi, host.ctx, "mon_a", { persistent: true });
    expect(host.notices.filter((n) => n.text.includes("Idle cache warming"))).toEqual([]);
    expect(await pi.emit("cache_warming_decision", { action: "warm" }, host.ctx)).toEqual([{ action: "stop" }]);
    // Model change ends it.
    await pi.emit("model_select", { model: host.ctx.model, source: "set" }, host.ctx);
    expect(await pi.emit("cache_warming_decision", { action: "warm" }, host.ctx)).toEqual([]);
    await pi.emit("session_shutdown", { reason: "quit" });
    expect(host.ctx.modelRegistry.streamSimple).not.toHaveBeenCalled();
  });
});
