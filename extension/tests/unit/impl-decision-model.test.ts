import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import { loadConfig } from "../../src/config";
import {
  DECISION_MAX_CHARS,
  DECISION_MAX_LINES,
  DecisionShadow,
  formatDecisionShadow,
  type ClassifierContextLike,
  type ClassifierRegistryLike,
  type ClassifierResultLike,
  type DecisionModelConfig,
  type DecisionRecord,
} from "../../src/decision-model";
import type { ManagerClient } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import type { OverloadControllerOptions } from "../../src/monitor-batching";
import { NotifyCenter } from "../../src/notify";
import { formatFamulusInfo } from "../../src/tui/famulus-command";
import type { FamulusWake } from "../../src/wake";

// pi's real registry must satisfy the structural surface the shadow uses.
type PiModelRegistry = ExtensionContext["modelRegistry"];
const _registryShape = (r: PiModelRegistry): ClassifierRegistryLike => r;
void _registryShape;

const JEV = { provider: "typesafe", id: "jev-1", type: "classifier" };

async function settle(): Promise<void> {
  for (let i = 0; i < 5; i++) await new Promise((resolve) => setImmediate(resolve));
}

function okResult(probability: number): ClassifierResultLike {
  return {
    provider: "typesafe",
    model: "jev-1",
    answers: { needs_attention: { type: "bool", probability } },
    usage: { input: 120, output: 1, cost: { total: 0.00001 } },
    stopReason: "stop",
  };
}

function fakeRegistry(opts: {
  available?: readonly typeof JEV[];
  classify?: (model: unknown, context: ClassifierContextLike, options?: { signal?: AbortSignal }) => Promise<ClassifierResultLike>;
  noClassify?: boolean;
} = {}) {
  const models = [JEV, { provider: "openrouter", id: "meta/jev-x", type: "classifier" }];
  const registry = {
    getAvailableOfType: vi.fn(async (_type: "classifier") => opts.available ?? [JEV]),
    getModelOfType: vi.fn((_type: "classifier", provider: string, id: string) => models.find((m) => m.provider === provider && m.id === id)),
    classify: opts.noClassify ? undefined : vi.fn(opts.classify ?? (async () => okResult(0.9))),
  };
  return registry;
}

function shadowFor(config: Partial<DecisionModelConfig>, registry: unknown, clock = new ManualClock(1_000)) {
  const records: DecisionRecord[] = [];
  const events: { type: string; fields?: Record<string, unknown> }[] = [];
  const shadow = new DecisionShadow({
    config: { enabled: true, timeoutMs: 3000, ...config },
    getRegistry: () => registry,
    sessionId: () => "sess_1",
    logPath: "/unused",
    clock,
    append: (line) => { records.push(JSON.parse(line) as DecisionRecord); },
    logEvent: (type, fields) => events.push({ type, fields }),
  });
  return { shadow, records, events, clock };
}

async function monitorSetup(shadow: DecisionShadow | null, clock: ManualClock, overload?: Partial<OverloadControllerOptions>) {
  const sent: { details?: unknown }[] = [];
  const manager = {
    ensureAvailable: async () => true,
    start: async () => ({ task_id: "mon_1", pid: 1 }),
    watch: async () => {},
    stop: async () => {},
  } as unknown as ManagerClient;
  const center = new NotifyCenter({ sendMessage: (message) => sent.push(message), isIdle: () => true, clock });
  const registry = new MonitorRegistry({
    getClient: () => manager,
    sessionEnv: () => ({}),
    getNotifyCenter: () => center,
    trackTask: () => {},
    clock,
    decision: shadow,
    ...(overload ? { overload } : {}),
  });
  const { taskId } = await registry.start(
    { command: "tail -f app.log", description: "errors in app.log", persistent: true, dedup_window_ms: 0 },
    { cwd: "/tmp" } as ExtensionContext,
  );
  const wakes = () => sent
    .map((item) => item.details as FamulusWake | undefined)
    .filter((d): d is Extract<FamulusWake, { kind: "monitor" }> => d?.kind === "monitor");
  return { registry, center, sent, taskId, wakes };
}

describe("decision model shadow: monitor wake path", () => {
  it("flag off: wakes are delivered and the classifier is never consulted", async () => {
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({ enabled: false }, fake);
    const clock = new ManualClock();
    const { registry, center, taskId, wakes } = await monitorSetup(shadow, clock);
    registry.handleOutput(taskId, "ERROR boom\n");
    clock.advanceBy(200);
    await settle();
    expect(wakes()).toHaveLength(1);
    expect(fake.getAvailableOfType).not.toHaveBeenCalled();
    expect(fake.classify).not.toHaveBeenCalled();
    expect(records).toEqual([]);
    center.dispose();
    registry.disposeAll();
  });

  it("no classifier available, or a pi without classify: no calls, no records, no errors, wake unchanged", async () => {
    for (const fake of [fakeRegistry({ available: [] }), fakeRegistry({ noClassify: true })]) {
      const clock = new ManualClock();
      const { shadow, records, events } = shadowFor({}, fake, clock);
      const { registry, center, taskId, wakes } = await monitorSetup(shadow, clock);
      registry.handleOutput(taskId, "ERROR boom\n");
      clock.advanceBy(200);
      await settle();
      registry.handleOutput(taskId, "ERROR again\n");
      clock.advanceBy(200);
      await settle();
      expect(wakes()).toHaveLength(2);
      expect(fake.classify ?? vi.fn()).not.toHaveBeenCalled();
      expect(records).toEqual([]);
      expect(events.filter((e) => e.type === "decision.error")).toEqual([]);
      expect(events.filter((e) => e.type === "decision.unavailable")).toHaveLength(1);
      expect(shadow.stats().unavailable).toBeTruthy();
      center.dispose();
      registry.disposeAll();
    }
  });

  it("delivers the wake before and independently of a classifier that never answers", async () => {
    const clock = new ManualClock();
    let sentWhenClassified = -1;
    let signal: AbortSignal | undefined;
    const holder: { sent?: unknown[] } = {};
    const fake = fakeRegistry({
      classify: (_m, _c, options) => {
        sentWhenClassified = holder.sent?.length ?? -1;
        signal = options?.signal;
        return new Promise(() => {});
      },
    });
    const { shadow, records } = shadowFor({ timeoutMs: 3000 }, fake, clock);
    const { registry, center, taskId, wakes, sent } = await monitorSetup(shadow, clock);
    holder.sent = sent;
    registry.handleOutput(taskId, "ERROR boom\n");
    clock.advanceBy(200);
    // Synchronously after the batch flush, before any classifier work ran.
    expect(wakes()).toHaveLength(1);
    expect(fake.classify).not.toHaveBeenCalled();
    await settle();
    expect(fake.classify).toHaveBeenCalledTimes(1);
    expect(sentWhenClassified).toBe(1);
    expect(records).toEqual([]);
    clock.advanceBy(3000);
    await settle();
    expect(signal?.aborted).toBe(true);
    expect(records).toHaveLength(1);
    expect(records[0]).toMatchObject({ taskId, kind: "batch", lines: 1, model: "typesafe/jev-1", probability: null, needsAttention: null, latencyMs: 3000, error: "timeout after 3000ms" });
    // A later batch still wakes immediately while the earlier one hung.
    registry.handleOutput(taskId, "ERROR again\n");
    clock.advanceBy(200);
    expect(wakes()).toHaveLength(2);
    center.dispose();
    registry.disposeAll();
  });

  it("does not hold a wake while classifier resolution is still pending", async () => {
    const clock = new ManualClock();
    const fake = fakeRegistry();
    fake.getAvailableOfType.mockImplementation(() => new Promise(() => {}));
    const { shadow } = shadowFor({}, fake, clock);
    const { registry, center, taskId, wakes } = await monitorSetup(shadow, clock);
    registry.handleOutput(taskId, "ERROR boom\n");
    clock.advanceBy(200);
    await settle();
    expect(wakes()).toHaveLength(1);
    expect(fake.classify).not.toHaveBeenCalled();
    center.dispose();
    registry.disposeAll();
  });

  it("terminal exit and timeout notices are not classified; stopped-by-model batches are not either", async () => {
    const clock = new ManualClock();
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({}, fake, clock);
    const { registry, center, taskId, sent } = await monitorSetup(shadow, clock);
    registry.handleExit(taskId, { event: "task_exited", task_id: taskId, exit_code: 1, duration_ms: 50 });
    center.settled();
    clock.advanceBy(10_000);
    await settle();
    expect(sent.length).toBeGreaterThan(0);
    expect(fake.classify).not.toHaveBeenCalled();

    const timed = new MonitorRegistry({
      getClient: () => ({ ensureAvailable: async () => true, start: async () => ({ task_id: "mon_2", pid: 2 }), watch: async () => {}, stop: async () => {} }) as unknown as ManagerClient,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
      decision: shadow,
    });
    await timed.start({ command: "sleep 99", description: "sleeper", timeout_ms: 1000 }, { cwd: "/tmp" } as ExtensionContext);
    clock.advanceBy(1000);
    await settle();
    clock.advanceBy(10_000);
    await settle();
    expect(fake.classify).not.toHaveBeenCalled();

    const stopping = await monitorSetup(shadow, clock);
    stopping.registry.handleOutput(stopping.taskId, "line before stop\n");
    stopping.registry.noteStopRequested(stopping.taskId);
    clock.advanceBy(200);
    await settle();
    expect(fake.classify).not.toHaveBeenCalled();
    expect(records).toEqual([]);
    stopping.center.dispose();
    stopping.registry.disposeAll();
    center.dispose();
    registry.disposeAll();
    timed.disposeAll();
  });

  it("classifies overload summaries but not the enter/leave notices", async () => {
    const clock = new ManualClock();
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({}, fake, clock);
    const { registry, center, taskId, wakes } = await monitorSetup(shadow, clock, {
      exitQuietMs: 3_600_000,
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      cadenceMs: [5_000, 5_000, 5_000, 5_000],
    });
    for (let i = 0; i < 4; i++) {
      registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
      clock.advanceBy(1_000);
      await settle();
    }
    clock.advanceBy(5_000);
    await settle();
    const texts = wakes().map((w) => w.event);
    expect(texts.some((t) => t.includes("overload summary"))).toBe(true);
    expect(texts.some((t) => t.includes("entering summary mode"))).toBe(true);
    const states = fake.classify!.mock.calls.map((call) => call[1].state);
    expect(states.some((s) => s.output_kind === "summary" && String(s.output).includes("overload summary"))).toBe(true);
    expect(states.some((s) => String(s.output).includes("entering summary mode"))).toBe(false);
    expect(records.some((r) => r.kind === "summary")).toBe(true);
    center.dispose();
    registry.disposeAll();
  });
});

describe("decision model shadow: classifier calls and records", () => {
  it("records answer, probability, latency and usage; counts would-suppress", async () => {
    const fake = fakeRegistry({ classify: async () => okResult(0.2) });
    const { shadow, records } = shadowFor({}, fake);
    shadow.observe({ taskId: "mon_1", description: "errors in app.log", command: "tail -f app.log", text: "GET /health 200\nGET /health 200", kind: "batch" });
    await settle();
    await shadow.flushed();
    expect(records).toEqual([expect.objectContaining({
      v: 1, sessionId: "sess_1", taskId: "mon_1", kind: "batch", lines: 2, model: "typesafe/jev-1",
      needsAttention: false, probability: 0.2, latencyMs: 0, usage: { input: 120, output: 1, costTotal: 0.00001 }, stopReason: "stop", error: null,
    })]);
    expect(shadow.stats()).toMatchObject({ decisions: 1, wouldSuppress: 1, errors: 0 });
    const [, context] = fake.classify!.mock.calls[0];
    expect(context.questions).toEqual({
      needs_attention: {
        type: "bool",
        instructions: "Does this monitor output show a problem or event the agent watching 'errors in app.log' must act on now?",
        criteria: { true: expect.any(String), false: expect.any(String) },
      },
    });
    expect(context.state).toMatchObject({ monitor_description: "errors in app.log", command: "tail -f app.log", output: "GET /health 200\nGET /health 200" });
  });

  it("keeps at most the last DECISION_MAX_LINES lines of a many-short-lines batch", async () => {
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({}, fake);
    const text = Array.from({ length: 1000 }, (_, i) => `l${i}`).join("\n");
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text, kind: "batch" });
    await settle();
    const output = String(fake.classify!.mock.calls[0][1].state.output);
    expect(output.split("\n")).toHaveLength(DECISION_MAX_LINES);
    expect(output.endsWith("l999")).toBe(true);
    expect(records[0]).toMatchObject({ lines: 1000, truncated: true });
  });

  it("observe() with the flag off does nothing", async () => {
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({ enabled: false }, fake);
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "x", kind: "batch" });
    await settle();
    expect(fake.getAvailableOfType).not.toHaveBeenCalled();
    expect(fake.classify).not.toHaveBeenCalled();
    expect(records).toEqual([]);
  });

  it("records error results and thrown errors without surfacing them", async () => {
    const results: (() => Promise<ClassifierResultLike>)[] = [
      async () => ({ answers: {}, stopReason: "error", errorMessage: "401 unauthorized" }),
      async () => { throw new Error("socket hang up"); },
      () => { throw new Error("sync boom"); },
    ];
    const fake = fakeRegistry({ classify: () => results.shift()!() });
    const { shadow, records } = shadowFor({}, fake);
    for (let i = 0; i < 3; i++) {
      shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: `x${i}`, kind: "batch" });
      await settle();
    }
    await shadow.flushed();
    expect(records.map((r) => r.error)).toEqual(["401 unauthorized", "Error: socket hang up", "Error: sync boom"]);
    expect(records.every((r) => r.probability === null && r.needsAttention === null)).toBe(true);
    expect(shadow.stats()).toMatchObject({ decisions: 0, errors: 3 });
  });

  it("uses the configured provider/id (ids may contain a slash) and never lists available models", async () => {
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({ model: "openrouter/meta/jev-x" }, fake);
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "x", kind: "batch" });
    await settle();
    expect(fake.getModelOfType).toHaveBeenCalledWith("classifier", "openrouter", "meta/jev-x");
    expect(fake.getAvailableOfType).not.toHaveBeenCalled();
    expect(fake.classify!.mock.calls[0][0]).toMatchObject({ provider: "openrouter", id: "meta/jev-x" });
    expect(records[0]?.error).toBeNull();
  });

  it("a configured model that is not in the registry makes no calls", async () => {
    const fake = fakeRegistry();
    const { shadow, records, events } = shadowFor({ model: "typesafe/missing" }, fake);
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "x", kind: "batch" });
    await settle();
    expect(fake.classify).not.toHaveBeenCalled();
    expect(records).toEqual([]);
    expect(events).toContainEqual({ type: "decision.unavailable", fields: { reason: "configured classifier typesafe/missing not found" } });
  });

  it("bounds the state sent to the provider", async () => {
    const fake = fakeRegistry();
    const { shadow, records } = shadowFor({}, fake);
    const text = Array.from({ length: 1000 }, (_, i) => `line ${i} ${"x".repeat(300)}`).join("\n");
    shadow.observe({ taskId: "mon_1", description: "d".repeat(5000), command: "c".repeat(5000), text, kind: "batch" });
    await settle();
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "next", kind: "batch" });
    await settle();
    const [first, second] = fake.classify!.mock.calls.map((call) => call[1].state);
    const output = String(first.output);
    expect(output.length).toBeLessThanOrEqual(DECISION_MAX_CHARS);
    expect(output.split("\n").length).toBeLessThanOrEqual(DECISION_MAX_LINES);
    expect(output.endsWith("line 999 " + "x".repeat(300))).toBe(true);
    expect(String(first.monitor_description).length).toBeLessThanOrEqual(500);
    expect(String(first.command).length).toBeLessThanOrEqual(500);
    expect(JSON.stringify(first).length).toBeLessThan(6000);
    expect(String(second.previous_output).length).toBeLessThanOrEqual(500);
    expect(records[0]).toMatchObject({ lines: 1000, truncated: true });
  });

  it("dispose aborts in-flight work and records nothing afterwards", async () => {
    let signal: AbortSignal | undefined;
    const fake = fakeRegistry({ classify: (_m, _c, o) => { signal = o?.signal; return new Promise(() => {}); } });
    const { shadow, records, clock } = shadowFor({}, fake);
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "x", kind: "batch" });
    await settle();
    shadow.dispose();
    expect(signal?.aborted).toBe(true);
    clock.advanceBy(10_000);
    await settle();
    expect(records).toEqual([]);
  });

  it("writes JSONL to the log path by default", async () => {
    const dir = mkdtempSync(join(tmpdir(), "famulus-decision-"));
    const logPath = join(dir, "decision-shadow.jsonl");
    const shadow = new DecisionShadow({
      config: { enabled: true, timeoutMs: 3000 },
      getRegistry: () => fakeRegistry(),
      sessionId: () => "s",
      logPath,
      clock: new ManualClock(),
    });
    shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: "a\nb", kind: "batch" });
    await settle();
    await shadow.flushed();
    const lines = readFileSync(logPath, "utf8").trim().split("\n").map((l) => JSON.parse(l) as DecisionRecord);
    expect(lines).toEqual([expect.objectContaining({ taskId: "mon_1", lines: 2, needsAttention: true, probability: 0.9 })]);
  });
});

describe("decisionModel config", () => {
  function load(json: unknown) {
    const home = mkdtempSync(join(tmpdir(), "famulus-config-"));
    writeFileSync(join(home, "config.json"), JSON.stringify(json));
    return loadConfig(home);
  }

  it("is off by default and when malformed", () => {
    expect(load({}).decisionModel?.enabled ?? false).toBe(false);
    expect(load({ decisionModel: "yes" }).decisionModel?.enabled ?? false).toBe(false);
    expect(load({ decisionModel: { enabled: "true" } }).decisionModel?.enabled).toBe(false);
  });

  it("parses enabled, model and a clamped timeout", () => {
    expect(load({ decisionModel: { enabled: true } }).decisionModel).toEqual({ enabled: true, timeoutMs: 3000 });
    expect(load({ decisionModel: { enabled: true, model: "typesafe/jev-1", timeoutMs: 5 } }).decisionModel)
      .toEqual({ enabled: true, model: "typesafe/jev-1", timeoutMs: 100 });
    expect(load({ decisionModel: { enabled: true, model: "noslash", timeoutMs: 1e9 } }).decisionModel)
      .toEqual({ enabled: true, timeoutMs: 60_000 });
  });
});

describe("decision model status line", () => {
  it("summarizes would-suppress counts in /famulus only when enabled", async () => {
    const results = [0.1, 0.8, 0.3];
    const fake = fakeRegistry({ classify: async () => okResult(results.shift()!) });
    const { shadow } = shadowFor({}, fake);
    for (let i = 0; i < 3; i++) {
      shadow.observe({ taskId: "mon_1", description: "d", command: "c", text: `x${i}`, kind: "batch" });
      await settle();
    }
    const line = formatDecisionShadow(shadow, "/h/decision-shadow.jsonl");
    expect(line).toBe("shadow · typesafe/jev-1 · would suppress 2 of 3 · /h/decision-shadow.jsonl");
    const base = { extensionVersion: "1", configPath: "/h/config.json", managerPath: null, searched: "x", binaryVersion: null, daemon: null, daemonError: "", messaging: null };
    expect(formatFamulusInfo({ ...base, decisionModel: line }).split("\n")).toContain(`Decision model: ${line}`);
    expect(formatFamulusInfo(base)).not.toContain("Decision model");
  });
});
