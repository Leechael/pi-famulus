import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { ManualClock } from "../../src/clock";
import {
  CACHE_WARM_ENTRY_TYPE,
  CacheWarmController,
  buildWarmPayload,
  registerCacheWarming,
  type CacheWarmingSettings,
} from "../../src/cache-warm";
import { DEFAULT_CACHE_WARMING, loadConfig, resolveCacheWarmingConfig } from "../../src/config";

const TTL_S = 300; // 5 min -> warm at 270s
const DELAY = 270_000;

type Usage = { input?: number; output?: number; cacheRead?: number; cacheWrite?: number; cost?: { total: number } };
type Reply = { stopReason: string; errorMessage?: string; usage?: Usage };

const HIT: Reply = { stopReason: "length", usage: { input: 3, output: 1, cacheRead: 50_000, cacheWrite: 0, cost: { total: 0.015 } } };

/** Payload pi-ai's builder would produce for the replay's minimal context and maxTokens 1. */
function freshPayloadFor(api: string): Record<string, unknown> {
  switch (api) {
    case "anthropic-messages": return { model: "claude-x", max_tokens: 1, messages: [{ role: "user", content: "." }] };
    case "openai-responses": return { model: "claude-x", max_output_tokens: 16, input: [] };
    case "openai-completions": return { model: "claude-x", max_completion_tokens: 1, messages: [] };
    case "bedrock-converse-stream": return { modelId: "claude-x", inferenceConfig: { maxTokens: 1 }, messages: [] };
    default: return {};
  }
}

function realPayload(extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    model: "claude-x",
    max_tokens: 32_000,
    system: [{ type: "text", text: "big system prompt", cache_control: { type: "ephemeral" } }],
    messages: [{ role: "user", content: "hello" }],
    stream: true,
    ...extra,
  };
}

function setup(opts: {
  enabled?: boolean;
  maxIdleMs?: number;
  persistent?: boolean;
  hasUI?: boolean;
  model?: Record<string, unknown>;
  env?: NodeJS.ProcessEnv;
} = {}) {
  const clock = new ManualClock(1_000_000);
  const settings: CacheWarmingSettings = { enabled: opts.enabled ?? true, maxIdleMs: opts.maxIdleMs ?? 6 * 3_600_000 };
  let persistent = opts.persistent ?? true;
  const events: { type: string; fields?: Record<string, unknown> }[] = [];
  const entries: { type: string; data: any }[] = [];
  const notify = vi.fn();
  const calls: { model: any; options: any; payload: unknown }[] = [];
  const replies: (Reply | Promise<Reply>)[] = [];
  let idle = true;
  const ctx = {
    model: { provider: "anthropic", id: "claude-x", api: "anthropic-messages", promptCache: { short: TTL_S }, ...opts.model },
    hasUI: opts.hasUI ?? true,
    ui: { notify },
    isIdle: () => idle,
    sessionManager: { getSessionId: () => "sess-1" },
    modelRegistry: {
      streamSimple: (model: any, _context: unknown, options: any) => {
        const call = { model, options, payload: undefined as unknown };
        calls.push(call);
        const reply = replies.shift() ?? HIT;
        const run = (async (): Promise<Reply> => {
          call.payload = await options.onPayload(freshPayloadFor(model.api), model);
          const r = await reply;
          if (options.signal?.aborted) return { stopReason: "aborted" };
          return r;
        })().catch((e: Error) => ({ stopReason: "error", errorMessage: e.message }));
        return { result: () => run };
      },
    },
  };
  const controller = new CacheWarmController({
    clock,
    getSettings: () => settings,
    hasPersistentMonitor: () => persistent,
    logEvent: (type, fields) => events.push({ type, fields }),
    appendEntry: (type, data) => entries.push({ type, data }),
    configPath: "/home/u/.pi/agent/pi-famulus/config.json",
    env: () => opts.env ?? {},
  });
  const monitors = () => (persistent ? [{ taskId: "mon_1", persistent: true }] : []);
  const turn = (payload: unknown = realPayload()) => {
    idle = false;
    controller.onAgentStart(ctx as never);
    controller.onProviderRequest(payload, ctx as never);
  };
  const settle = () => {
    idle = true;
    controller.onAgentSettled(ctx as never);
  };
  return {
    clock, settings, ctx, controller, events, entries, notify, calls, replies,
    setPersistent: (value: boolean) => {
      persistent = value;
      controller.onMonitorsChanged(monitors(), ctx as never);
    },
    setIdle: (value: boolean) => { idle = value; },
    turn,
    settle,
    monitors,
  };
}

const flush = async () => {
  for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r));
};

describe("cache warming reminder", () => {
  it("notifies once per session and model when a persistent monitor starts without a TTL", () => {
    const t = setup({ model: { promptCache: undefined } });
    t.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }], t.ctx as never);
    t.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }, { taskId: "mon_2", persistent: true }], t.ctx as never);
    expect(t.notify).toHaveBeenCalledTimes(1);
    const [text, level] = t.notify.mock.calls[0];
    expect(level).toBe("info");
    expect(text).toContain("promptCache.short for anthropic/claude-x");
    expect(text).toContain("docs/models.md#prompt-cache-lifetimes");
    expect(text).not.toContain("cacheWarming");
    // Another model in the same session gets its own reminder.
    t.ctx.model = { ...t.ctx.model, id: "claude-y" };
    t.controller.onMonitorsChanged([{ taskId: "mon_3", persistent: true }], t.ctx as never);
    expect(t.notify).toHaveBeenCalledTimes(2);
  });

  it("names the pi-famulus flag when the TTL is known but warming is off", () => {
    const t = setup({ enabled: false });
    t.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }], t.ctx as never);
    expect(t.notify).toHaveBeenCalledTimes(1);
    expect(t.notify.mock.calls[0][0]).toContain('"cacheWarming": { "enabled": true }');
    expect(t.notify.mock.calls[0][0]).not.toContain("promptCache.short");
  });

  it("uses the long tier when PI_CACHE_RETENTION=long", () => {
    const t = setup({ env: { PI_CACHE_RETENTION: "long" } }); // model declares only short
    t.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }], t.ctx as never);
    expect(t.notify.mock.calls[0][0]).toContain("promptCache.long");
  });

  it("stays silent without UI, for timed monitors, and when warming is fully configured", () => {
    const noUi = setup({ enabled: false, hasUI: false });
    noUi.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }], noUi.ctx as never);
    expect(noUi.notify).not.toHaveBeenCalled();

    const timed = setup({ enabled: false });
    timed.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: false }], timed.ctx as never);
    expect(timed.notify).not.toHaveBeenCalled();

    const ready = setup({ enabled: true });
    ready.controller.onMonitorsChanged([{ taskId: "mon_1", persistent: true }], ready.ctx as never);
    expect(ready.notify).not.toHaveBeenCalled();
  });
});

describe("real-request discriminator", () => {
  it("captures a running-agent session turn and never modifies the payload", async () => {
    const t = setup();
    const payload = realPayload();
    const before = structuredClone(payload);
    t.turn(payload);
    t.settle();
    expect(payload).toEqual(before);
    expect(t.controller.status().state).toBe("scheduled");
  });

  it("ignores pi's idle warm replays and streaming-phase 1-token replays", () => {
    const t = setup();
    t.turn();
    t.settle();
    const scheduledAt = t.controller.status().nextWarmAt;
    t.clock.advanceBy(60_000);
    // pi idle warm: agent idle
    t.controller.onProviderRequest(realPayload({ max_tokens: 1 }), t.ctx as never);
    // pi streaming-phase warm while a run is active, without agent_start (cap 1)
    t.setIdle(false);
    t.controller.onProviderRequest(realPayload({ max_tokens: 1 }), t.ctx as never);
    t.setIdle(true);
    expect(t.controller.status()).toMatchObject({ state: "scheduled", nextWarmAt: scheduledAt });
  });

  it("ignores an idle-phase replay even when it carries no 1-token cap", () => {
    // openai-responses without supportsMaxOutputTokens: pi's warm payload has no cap at all.
    const t = setup({ model: { api: "openai-responses" } });
    t.turn({ model: "claude-x", input: [] });
    t.settle();
    const at = t.controller.status().nextWarmAt;
    t.clock.advanceBy(60_000);
    t.controller.onProviderRequest({ model: "claude-x", input: [] }, t.ctx as never); // agent idle
    expect(t.controller.status()).toMatchObject({ state: "scheduled", nextWarmAt: at });
  });

  it("treats a Responses replay clamped to 16 output tokens as a warm, not a turn", () => {
    const t = setup({ model: { api: "openai-responses" } });
    t.turn({ model: "claude-x", input: [] }); // real turn: no cap
    t.settle();
    const at = t.controller.status().nextWarmAt;
    t.clock.advanceBy(10_000);
    t.setIdle(false);
    t.controller.onProviderRequest({ model: "claude-x", input: [], max_output_tokens: 16 }, t.ctx as never);
    t.setIdle(true);
    expect(t.controller.status().nextWarmAt).toBe(at);
  });

  it.each([
    ["Anthropic thread continuation", {}, realPayload({ thread: { type: "continue" } }), "thread"],
    ["previous_response_id", { api: "openai-responses" }, { model: "claude-x", previous_response_id: "r1" }, "previous response"],
    ["budget thinking", {}, realPayload({ thinking: { type: "enabled", budget_tokens: 8000 } }), "budget-based thinking"],
    ["codex", { api: "openai-codex-responses" }, { model: "claude-x" }, "openai-codex-responses"],
    ["google", { api: "google-generative-ai" }, { model: "claude-x" }, "google"],
    ["unknown api", { api: "my-custom-api" }, { model: "claude-x" }, "not supported"],
    ["redirected model", {}, realPayload({ model: "claude-other" }), "not the selected model"],
    ["no promptCache", { promptCache: undefined }, realPayload(), "no promptCache.short"],
  ])("refuses to warm: %s", (_name, model, payload, reason) => {
    const t = setup({ model: model as Record<string, unknown> });
    t.turn(payload);
    t.settle();
    const status = t.controller.status();
    expect(status.state).toBe("inactive");
    expect(status.reason).toContain(reason);
    t.clock.advanceBy(3_600_000);
    expect(t.calls).toHaveLength(0);
  });

  it("allows adaptive thinking", () => {
    const t = setup();
    t.turn(realPayload({ thinking: { type: "adaptive" } }));
    t.settle();
    expect(t.controller.status().state).toBe("scheduled");
  });
});

describe("warm schedule", () => {
  it("warms at min(0.9*TTL, TTL-10s) after the real request, then from each successful warm", async () => {
    const t = setup();
    const t0 = t.clock.now();
    t.turn();
    t.clock.advanceBy(30_000); // the run streams for 30s
    t.settle();
    expect(t.controller.status().nextWarmAt).toBe(t0 + DELAY);
    t.clock.advanceBy(DELAY - 30_000 - 1);
    expect(t.calls).toHaveLength(0);
    t.clock.advanceBy(1);
    expect(t.calls).toHaveLength(1);
    await flush();
    expect(t.controller.status()).toMatchObject({ state: "scheduled", nextWarmAt: t0 + 2 * DELAY });
    t.clock.advanceBy(DELAY);
    await flush();
    expect(t.calls).toHaveLength(2);
  });

  it("uses TTL-10s for short TTLs and 0.9*TTL for 1h", () => {
    const short = setup({ model: { promptCache: { short: 60 } } });
    const s0 = short.clock.now();
    short.turn();
    short.settle();
    expect(short.controller.status().nextWarmAt).toBe(s0 + 50_000);

    const long = setup({ model: { promptCache: { short: 300, long: 3600 } }, env: { PI_CACHE_RETENTION: "long" } });
    const l0 = long.clock.now();
    long.turn();
    long.settle();
    expect(long.controller.status().nextWarmAt).toBe(l0 + 3_240_000);
  });

  it("replays the captured payload with only the output cap changed, without continuation", async () => {
    const t = setup();
    const payload = realPayload();
    t.turn(payload);
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    const call = t.calls[0];
    expect(call.payload).toEqual({ ...payload, max_tokens: 1 });
    expect(call.options).toMatchObject({ maxTokens: 1, maxRetries: 0, disableContinuation: true, sessionId: "sess-1", cacheRetention: "short" });
    expect(call.model.id).toBe("claude-x");
  });

  it("does not schedule while no persistent monitor runs, and starts when one appears", async () => {
    const t = setup({ persistent: false });
    t.turn();
    t.settle();
    expect(t.controller.status()).toMatchObject({ state: "inactive", reason: "no persistent monitor" });
    t.clock.advanceBy(60_000);
    t.setPersistent(true);
    expect(t.controller.status().state).toBe("scheduled");
    t.clock.advanceBy(DELAY - 60_000);
    await flush();
    expect(t.calls).toHaveLength(1);
  });
});

describe("stop conditions", () => {
  it("stops when the last persistent monitor ends", () => {
    const t = setup();
    t.turn();
    t.settle();
    t.setPersistent(false);
    expect(t.controller.status()).toMatchObject({ state: "inactive", reason: "no persistent monitor" });
    t.clock.advanceBy(3_600_000);
    expect(t.calls).toHaveLength(0);
  });

  it("stops after maxIdleMs since the last real request", async () => {
    const t = setup({ maxIdleMs: 3 * DELAY });
    t.turn();
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    t.clock.advanceBy(DELAY);
    await flush();
    t.clock.advanceBy(DELAY);
    await flush();
    expect(t.calls).toHaveLength(3);
    expect(t.controller.status()).toMatchObject({ state: "inactive", reason: "maxIdleMs reached" });
    t.clock.advanceBy(10 * DELAY);
    await flush();
    expect(t.calls).toHaveLength(3);
  });

  it("a new real request aborts an in-flight warm and reschedules from itself", async () => {
    const t = setup();
    let release!: (r: Reply) => void;
    t.replies.push(new Promise<Reply>((r) => { release = r; }));
    t.turn();
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    expect(t.controller.status().state).toBe("warming");
    const signal = t.calls[0].options.signal as AbortSignal;
    const t1 = t.clock.now();
    t.turn(); // monitor wake: agent_start + new request
    expect(signal.aborted).toBe(true);
    release(HIT);
    await flush();
    expect(t.entries).toHaveLength(0); // aborted warm is not accounted
    t.settle();
    expect(t.controller.status().nextWarmAt).toBe(t1 + DELAY);
  });

  it("agent_start cancels a pending warm before the request is sent", () => {
    const t = setup();
    t.turn();
    t.settle();
    t.setIdle(false);
    t.controller.onAgentStart(t.ctx as never);
    expect(t.controller.status().state).toBe("inactive");
    t.clock.advanceBy(DELAY);
    expect(t.calls).toHaveLength(0);
  });

  it.each(["model changed", "thinking level changed", "session compacted", "session tree navigated", "session shutdown"])(
    "stops and drops the captured request on %s",
    (reason) => {
      const t = setup();
      t.turn();
      t.settle();
      t.controller.reset(reason);
      t.settle();
      t.setPersistent(true);
      expect(t.controller.status()).toMatchObject({ state: "inactive", reason });
      t.clock.advanceBy(3_600_000);
      expect(t.calls).toHaveLength(0);
    },
  );

  it("does not warm when the run outlived the cache entry", () => {
    const t = setup();
    t.turn();
    t.clock.advanceBy(TTL_S * 1000); // long tool run, no further request
    t.settle();
    expect(t.controller.status().reason).toContain("likely expired");
    t.clock.advanceBy(3_600_000);
    expect(t.calls).toHaveLength(0);
  });
});

describe("miss detection", () => {
  it("stops after a warm that wrote instead of read, until the next real request", async () => {
    const t = setup();
    t.replies.push({ stopReason: "length", usage: { input: 3, output: 1, cacheRead: 0, cacheWrite: 50_000, cost: { total: 0.19 } } });
    t.turn();
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    const status = t.controller.status();
    expect(status.state).toBe("inactive");
    expect(status.reason).toContain("missed the cache");
    expect(t.entries[0].data).toMatchObject({ hit: false, cost: 0.19 });
    t.setPersistent(true); // monitor churn does not restart a halted run
    expect(t.controller.status().state).toBe("inactive");
    t.clock.advanceBy(3_600_000);
    await flush();
    expect(t.calls).toHaveLength(1);
    t.turn();
    t.settle();
    expect(t.controller.status().state).toBe("scheduled");
  });

  it("stops after a failed warm", async () => {
    const t = setup();
    t.replies.push({ stopReason: "error", errorMessage: "overloaded" });
    t.turn();
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    expect(t.controller.status().reason).toBe("warm failed: overloaded");
    expect(t.entries).toHaveLength(0);
  });
});

describe("coordination with pi's warmer", () => {
  it("tells pi to stop only while pi-famulus owns idle warming", async () => {
    const t = setup();
    expect(t.controller.decide()).toBeUndefined(); // nothing captured
    t.turn();
    expect(t.controller.decide()).toBeUndefined(); // streaming phase: pi decides
    t.settle();
    expect(t.controller.decide()).toEqual({ action: "stop" });
    t.setPersistent(false);
    expect(t.controller.decide()).toBeUndefined();
  });

  it("stands down once warming is switched off", () => {
    const t = setup();
    t.turn();
    t.settle();
    t.settings.enabled = false;
    expect(t.controller.decide()).toBeUndefined();
  });

  it("never overrides pi when warming is disabled", () => {
    const t = setup({ enabled: false });
    t.turn();
    t.settle();
    expect(t.controller.decide()).toBeUndefined();
  });
});

describe("accounting", () => {
  it("records each warm as a log event and a session custom entry with running totals", async () => {
    const t = setup();
    t.turn();
    t.settle();
    t.clock.advanceBy(DELAY);
    await flush();
    t.clock.advanceBy(DELAY);
    await flush();
    expect(t.entries.map((e) => e.type)).toEqual([CACHE_WARM_ENTRY_TYPE, CACHE_WARM_ENTRY_TYPE]);
    expect(t.entries[1].data).toMatchObject({
      provider: "anthropic", model: "claude-x", hit: true, warms: 2,
      usage: { cacheRead: 50_000, cacheWrite: 0, output: 1 },
    });
    expect(t.entries[1].data.totalCost).toBeCloseTo(0.03);
    expect(t.events.filter((e) => e.type === "cache-warm.sent")).toHaveLength(2);
    expect(t.controller.status()).toMatchObject({ warms: 2, hits: 2 });
  });
});

describe("pi event routing", () => {
  function wired() {
    const handlers = new Map<string, (event: any, ctx: any) => unknown>();
    const pi = { on: (name: string, fn: any) => handlers.set(name, fn) };
    const t = setup();
    const controller = registerCacheWarming(pi as never, {
      clock: t.clock,
      getSettings: () => ({ enabled: true, maxIdleMs: 6 * 3_600_000 }),
      hasPersistentMonitor: () => true,
      logEvent: () => {},
      appendEntry: () => {},
      configPath: "/x/config.json",
    });
    const emit = (name: string, event: Record<string, unknown> = {}) => handlers.get(name)!({ type: name, ...event }, t.ctx);
    const scheduleOne = () => {
      t.setIdle(false);
      emit("agent_start");
      emit("before_provider_request", { payload: realPayload() });
      t.setIdle(true);
      emit("agent_settled", { aborted: false });
    };
    return { t, controller, emit, scheduleOne };
  }

  it("schedules from agent_start / before_provider_request / agent_settled and answers cache_warming_decision", () => {
    const w = wired();
    w.scheduleOne();
    expect(w.controller.status().state).toBe("scheduled");
    expect(w.emit("cache_warming_decision", { action: "warm" })).toEqual({ action: "stop" });
    w.t.setIdle(false);
    w.emit("agent_start");
    expect(w.controller.status().state).toBe("inactive");
  });

  it.each(["model_select", "thinking_level_select", "session_compact", "session_tree", "session_start", "session_shutdown"])(
    "%s stops warming and drops the captured request",
    (name) => {
      const w = wired();
      w.scheduleOne();
      w.emit(name);
      w.emit("agent_settled", { aborted: false });
      expect(w.controller.status().state).toBe("inactive");
    },
  );
});

describe("flag off", () => {
  it("captures nothing while off and reports why", () => {
    const t = setup({ enabled: false });
    t.turn();
    t.settle();
    expect(t.controller.status()).toMatchObject({ state: "inactive", reason: "cache warming disabled" });
    expect(t.events).toEqual([]);
    // Turning it on later does not warm a request captured while it was off.
    t.settings.enabled = true;
    t.settle();
    expect(t.controller.status().state).toBe("inactive");
    t.clock.advanceBy(3_600_000);
    expect(t.calls).toHaveLength(0);
  });


  it("leaves every pi hook a no-op", async () => {
    const handlers = new Map<string, (event: any, ctx: any) => unknown>();
    const pi = { on: (name: string, fn: any) => handlers.set(name, fn) };
    const t = setup({ enabled: false });
    const clock = t.clock;
    const controller = registerCacheWarming(pi as never, {
      clock,
      getSettings: () => ({ ...DEFAULT_CACHE_WARMING }),
      hasPersistentMonitor: () => true,
      logEvent: () => {},
      appendEntry: () => { throw new Error("no entries expected"); },
      configPath: "/x/config.json",
    });
    const payload = realPayload();
    const before = structuredClone(payload);
    t.setIdle(false);
    handlers.get("agent_start")!({ type: "agent_start" }, t.ctx);
    expect(handlers.get("before_provider_request")!({ type: "before_provider_request", payload }, t.ctx)).toBeUndefined();
    t.setIdle(true);
    handlers.get("agent_settled")!({ type: "agent_settled", aborted: false }, t.ctx);
    expect(payload).toEqual(before);
    expect(handlers.get("cache_warming_decision")!({ type: "cache_warming_decision", action: "warm" }, t.ctx)).toBeUndefined();
    clock.advanceBy(6 * 3_600_000);
    await flush();
    expect(t.calls).toHaveLength(0);
    expect(controller.status()).toMatchObject({ state: "inactive", warms: 0 });
  });
});

describe("output cap mapping", () => {
  it.each([
    ["anthropic-messages", { max_tokens: 32000, x: 1 }, { max_tokens: 1 }, { max_tokens: 1, x: 1 }],
    ["openai-responses", { input: [], x: 1 }, { max_output_tokens: 16 }, { input: [], x: 1, max_output_tokens: 16 }],
    ["azure-openai-responses", { max_output_tokens: 9000 }, { max_output_tokens: 16 }, { max_output_tokens: 16 }],
    ["openai-completions", { max_tokens: 9000 }, { max_completion_tokens: 1 }, { max_completion_tokens: 1 }],
    ["openai-completions", { messages: [] }, { max_tokens: 1 }, { messages: [], max_tokens: 1 }],
    ["bedrock-converse-stream", { inferenceConfig: { maxTokens: 9000, temperature: 0 } }, { inferenceConfig: { maxTokens: 1 } }, { inferenceConfig: { maxTokens: 1, temperature: 0 } }],
    ["mistral-conversations", { maxTokens: 9000 }, { maxTokens: 1 }, { maxTokens: 1 }],
  ])("%s", (api, captured, fresh, expected) => {
    const result = buildWarmPayload(api, captured, fresh);
    expect(result).toEqual({ ok: true, payload: expected });
    expect(captured).not.toEqual(expected); // the capture is not mutated
  });

  it("refuses when the builder set no cap", () => {
    expect(buildWarmPayload("openai-responses", { input: [] }, { input: [] })).toEqual({ ok: false, refused: "provider builder set no output cap" });
  });
});

describe("cacheWarming config", () => {
  it("defaults off with a 6h idle cap and validates fields", () => {
    expect(resolveCacheWarmingConfig({ ...loadConfig(mkdtempSync(join(tmpdir(), "fam-cw-"))) })).toEqual({ enabled: false, maxIdleMs: 21_600_000 });
    const home = mkdtempSync(join(tmpdir(), "fam-cw-"));
    writeFileSync(join(home, "config.json"), JSON.stringify({ cacheWarming: { enabled: true, maxIdleMs: 7_200_000 } }));
    expect(resolveCacheWarmingConfig(loadConfig(home))).toEqual({ enabled: true, maxIdleMs: 7_200_000 });
    writeFileSync(join(home, "config.json"), JSON.stringify({ cacheWarming: { enabled: "yes", maxIdleMs: -5 } }));
    expect(resolveCacheWarmingConfig(loadConfig(home))).toEqual({ enabled: false, maxIdleMs: 21_600_000 });
  });
});
