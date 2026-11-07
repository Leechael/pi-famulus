/**
 * Idle prompt-cache warming while persistent monitors keep the session open.
 *
 * pi's own warmer (core/cache-warmer.ts) stops 30 minutes after the last real
 * request, so a session parked behind a persistent monitor loses its cache
 * long before the monitor wakes it. With `cacheWarming.enabled`, pi-famulus
 * replays the last real session request with a one-token output cap shortly
 * before the provider's cache entry expires, for as long as a persistent
 * monitor is active and at most `maxIdleMs` after that request.
 *
 * TTLs come from the user's pi config (`promptCache` in models.json); this
 * module ships no TTL table and never writes pi config. Nothing here enters
 * the model's context.
 *
 * Real-request discriminator (before_provider_request carries only the
 * payload): pi emits that event for the agent loop's requests (session turns
 * and pi's own warm replays, which reuse the turn's options). Compaction and
 * branch summaries build their own options without `onPayload` and never
 * reach the event; our own warms pass their own `onPayload` through
 * `modelRegistry.streamSimple`, which bypasses it too. A request is a real
 * session turn when the agent is running (`ctx.isIdle()` is false; pi's idle
 * warms fire while idle) and its output cap is absent or above
 * WARM_OUTPUT_CAP_CEILING (pi's warm replays carry `maxTokens: 1`, clamped to
 * 16 by the OpenAI Responses builders).
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { Clock, ClockTimer } from "./clock";

/** pi's warm replays cap output at 1 token; the Responses APIs clamp that to 16. */
export const WARM_OUTPUT_CAP_CEILING = 16;

/** Custom session entry type for warm accounting (never sent to the model). */
export const CACHE_WARM_ENTRY_TYPE = "famulus-cache-warm";

export const PI_PROMPT_CACHE_DOCS = "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md#prompt-cache-lifetimes";

type CapPath = readonly string[];

/**
 * Where each provider API puts the output-token cap, from pi-ai's request
 * builders. The replay copies the cap the builder produced for `maxTokens: 1`
 * (so per-API clamps such as Responses' minimum of 16 are respected) onto the
 * captured payload at the same path. `exclusive` paths are alternatives: the
 * one the builder did not use is removed from the replay.
 */
const OUTPUT_CAP_PATHS: Record<string, { paths: CapPath[]; exclusive?: boolean }> = {
  "anthropic-messages": { paths: [["max_tokens"]] },
  "bedrock-converse-stream": { paths: [["inferenceConfig", "maxTokens"]] },
  "openai-completions": { paths: [["max_completion_tokens"], ["max_tokens"]], exclusive: true },
  "openai-responses": { paths: [["max_output_tokens"]] },
  "azure-openai-responses": { paths: [["max_output_tokens"]] },
  "mistral-conversations": { paths: [["maxTokens"]] },
};

/**
 * APIs we deliberately do not warm, with the reason recorded in status/logs.
 * Unknown/custom APIs are refused too ("api not supported").
 */
const REFUSED_APIS: Record<string, string> = {
  "openai-codex-responses": "openai-codex-responses sends no output cap and keeps server-side continuation",
  "google-generative-ai": "google payloads carry a live AbortSignal and use implicit caching",
  "google-vertex": "google payloads carry a live AbortSignal and use implicit caching",
  "pi-messages": "pi-messages wraps the transcript; the replay cannot be capped safely",
};

export interface CacheWarmingSettings {
  enabled: boolean;
  /** Stop warming this long after the last real request (ms). */
  maxIdleMs: number;
}

type WarmCtx = Pick<ExtensionContext, "model" | "modelRegistry" | "hasUI" | "ui" | "isIdle" | "sessionManager">;
type ModelLike = NonNullable<ExtensionContext["model"]>;

interface Captured {
  payload: unknown;
  model: ModelLike;
  api: string;
  tier: "short" | "long";
  ttlMs: number;
  delayMs: number;
  sessionId: string;
}

export interface CacheWarmStatus {
  state: "inactive" | "scheduled" | "warming";
  reason?: string;
  nextWarmAt?: number;
  warms: number;
  hits: number;
  totalCost: number;
}

export interface CacheWarmDeps {
  clock: Clock;
  getSettings(): CacheWarmingSettings;
  hasPersistentMonitor(): boolean;
  logEvent(type: string, fields?: Record<string, unknown>): void;
  appendEntry(customType: string, data: unknown): void;
  /** Path of pi-famulus config.json, named in the reminder. */
  configPath: string;
  env?: () => NodeJS.ProcessEnv;
}

/** Refresh at 90% of the TTL while keeping at least 10s of margin (pi's formula). */
export function warmDelayMs(ttlMs: number): number | undefined {
  if (ttlMs <= 10_000) return undefined;
  return Math.max(1, Math.floor(Math.min(ttlMs * 0.9, ttlMs - 10_000)));
}

/** pi's tier selection: session requests set no cacheRetention, so PI_CACHE_RETENTION decides. */
export function retentionTier(env: NodeJS.ProcessEnv): "short" | "long" {
  return env.PI_CACHE_RETENTION === "long" ? "long" : "short";
}

export function promptCacheTtlMs(model: ModelLike | undefined, tier: "short" | "long"): number | undefined {
  const seconds = (model as { promptCache?: { short?: number; long?: number } } | undefined)?.promptCache?.[tier];
  return typeof seconds === "number" && seconds > 0 ? seconds * 1000 : undefined;
}

function getPath(obj: unknown, path: CapPath): unknown {
  let cur = obj;
  for (const key of path) {
    if (typeof cur !== "object" || cur === null) return undefined;
    cur = (cur as Record<string, unknown>)[key];
  }
  return cur;
}

function setPath(obj: Record<string, unknown>, path: CapPath, value: unknown): void {
  let cur = obj;
  for (const key of path.slice(0, -1)) {
    const next = cur[key];
    if (typeof next !== "object" || next === null) cur[key] = {};
    cur = cur[key] as Record<string, unknown>;
  }
  cur[path[path.length - 1]] = value;
}

function deletePath(obj: Record<string, unknown>, path: CapPath): void {
  const parent = getPath(obj, path.slice(0, -1));
  if (typeof parent === "object" && parent !== null) delete (parent as Record<string, unknown>)[path[path.length - 1]];
}

/** Output cap the payload carries for this API, if any. */
export function outputCap(api: string, payload: unknown): number | undefined {
  const spec = OUTPUT_CAP_PATHS[api];
  if (!spec) return undefined;
  for (const path of spec.paths) {
    const value = getPath(payload, path);
    if (typeof value === "number") return value;
  }
  return undefined;
}

/** Why this payload must not be replayed, or undefined when it can be. */
export function replayRefusal(api: string, payload: unknown, model: ModelLike): string | undefined {
  if (REFUSED_APIS[api]) return REFUSED_APIS[api];
  if (!OUTPUT_CAP_PATHS[api]) return `api ${api} not supported`;
  if (typeof payload !== "object" || payload === null) return "payload is not an object";
  const p = payload as Record<string, unknown>;
  const payloadModel = typeof p.model === "string" ? p.model : typeof p.modelId === "string" ? p.modelId : undefined;
  if (payloadModel !== undefined && payloadModel !== model.id) return `request went to ${payloadModel}, not the selected model`;
  if (p.thread !== undefined) return "payload carries Anthropic thread continuation";
  if (p.previous_response_id !== undefined) return "payload continues a previous response";
  // Budget-based thinking derives budget_tokens from max_tokens (pi's isReplayable).
  const thinking = (api === "bedrock-converse-stream"
    ? getPath(p, ["additionalModelRequestFields", "thinking"])
    : p.thinking) as { type?: unknown } | undefined;
  if (thinking?.type === "enabled") return "budget-based thinking cannot be replayed with a one-token cap";
  return undefined;
}

/**
 * The captured payload with the output cap the provider builder chose for
 * `maxTokens: 1` (read from `fresh`). Returns a refusal string when the
 * builder set no cap at a known path.
 */
export function buildWarmPayload(
  api: string,
  captured: unknown,
  fresh: unknown,
): { ok: true; payload: unknown } | { ok: false; refused: string } {
  const spec = OUTPUT_CAP_PATHS[api];
  if (!spec) return { ok: false, refused: `api ${api} not supported` };
  const used = spec.paths.find((path) => typeof getPath(fresh, path) === "number");
  if (!used) return { ok: false, refused: "provider builder set no output cap" };
  const payload = structuredClone(captured) as Record<string, unknown>;
  if (spec.exclusive) for (const path of spec.paths) if (path !== used) deletePath(payload, path);
  setPath(payload, used, getPath(fresh, used));
  return { ok: true, payload };
}

/** Minimal transcript for the replay; its payload is replaced in onPayload. */
function minimalContext(now: number) {
  return { systemPrompt: "", messages: [{ role: "user" as const, content: [{ type: "text" as const, text: "." }], timestamp: now }] };
}

export class CacheWarmController {
  private readonly deps: CacheWarmDeps;
  private ctx: WarmCtx | null = null;
  private captured: Captured | null = null;
  /** Why the last real request cannot be warmed. */
  private refusal: string | null = "no real request yet";
  /** Set when warming stopped until the next real request. */
  private halted: string | null = null;
  private lastRealAt = 0;
  private lastWarmAt = 0;
  private agentRunning = false;
  private timer: ClockTimer | null = null;
  private nextWarmAt: number | undefined;
  private refreshDeadlineAt = 0;
  private inFlight: AbortController | null = null;
  private inactiveReason = "no real request yet";
  private readonly reminded = new Set<string>();
  private readonly seenPersistent = new Set<string>();
  private warms = 0;
  private hits = 0;
  private totalCost = 0;

  constructor(deps: CacheWarmDeps) {
    this.deps = deps;
  }

  private settings(): CacheWarmingSettings {
    return this.deps.getSettings();
  }

  private env(): NodeJS.ProcessEnv {
    return this.deps.env?.() ?? process.env;
  }

  status(): CacheWarmStatus {
    const base = { warms: this.warms, hits: this.hits, totalCost: this.totalCost };
    if (this.inFlight) return { state: "warming", ...base };
    if (this.timer !== null) return { state: "scheduled", nextWarmAt: this.nextWarmAt, ...base };
    return { state: "inactive", reason: this.inactiveReason, ...base };
  }

  /** before_provider_request: capture real session turns. Never modifies the payload. */
  onProviderRequest(payload: unknown, ctx: WarmCtx): void {
    this.ctx = ctx;
    if (!this.settings().enabled) return;
    // pi's idle warm replays fire while the agent is idle; session turns never do.
    if (ctx.isIdle()) return;
    const model = ctx.model;
    const api = model?.api ?? "";
    const cap = outputCap(api, payload);
    // pi's streaming-phase warm replay: one-token cap (16 on Responses APIs).
    if (cap !== undefined && cap <= WARM_OUTPUT_CAP_CEILING) return;

    this.agentRunning = true;
    this.cancelWarm();
    this.halted = null;
    this.lastRealAt = this.deps.clock.now();
    this.lastWarmAt = 0;
    this.captured = null;
    const refusal = this.captureRefusal(payload, ctx);
    if (refusal) {
      if (refusal !== this.refusal) this.deps.logEvent("cache-warm.refused", { reason: refusal, model: model ? `${model.provider}/${model.id}` : null });
      this.refusal = refusal;
      this.inactiveReason = refusal;
      return;
    }
    this.refusal = null;
  }

  private captureRefusal(payload: unknown, ctx: WarmCtx): string | undefined {
    const model = ctx.model;
    if (!model) return "no model";
    const refusal = replayRefusal(model.api, payload, model);
    if (refusal) return refusal;
    const tier = retentionTier(this.env());
    const ttlMs = promptCacheTtlMs(model, tier);
    if (ttlMs === undefined) return `no promptCache.${tier} for ${model.provider}/${model.id}`;
    const delayMs = warmDelayMs(ttlMs);
    if (delayMs === undefined) return "cache lifetime too short to warm";
    let copy: unknown;
    try {
      copy = structuredClone(payload);
    } catch {
      return "payload cannot be copied";
    }
    this.captured = {
      payload: copy, model, api: model.api, tier, ttlMs, delayMs,
      sessionId: ctx.sessionManager.getSessionId(),
    };
    return undefined;
  }

  onAgentStart(ctx: WarmCtx): void {
    this.ctx = ctx;
    this.agentRunning = true;
    if (this.inFlight || this.timer !== null) this.inactiveReason = "agent running";
    this.cancelWarm();
  }

  onAgentSettled(ctx: WarmCtx): void {
    this.ctx = ctx;
    this.agentRunning = false;
    this.maybeSchedule();
  }

  /** Model, thinking level, compaction, tree navigation, session end: the captured prefix is no longer the next request's. */
  reset(reason: string): void {
    const wasActive = this.inFlight !== null || this.timer !== null;
    this.cancelWarm();
    this.captured = null;
    this.halted = null;
    this.refusal = reason;
    this.inactiveReason = reason;
    if (wasActive) this.deps.logEvent("cache-warm.stop", { reason });
  }

  onSessionStart(ctx: WarmCtx): void {
    this.ctx = ctx;
    this.agentRunning = false;
    this.reset("no real request yet");
  }

  /** Monitor set changed: reminder for newly started persistent monitors, then start/stop warming. */
  onMonitorsChanged(monitors: readonly { taskId: string; persistent: boolean }[], ctx: WarmCtx | null): void {
    if (ctx) this.ctx = ctx;
    const fresh = monitors.filter((m) => m.persistent && !this.seenPersistent.has(m.taskId));
    for (const m of fresh) this.seenPersistent.add(m.taskId);
    if (fresh.length > 0) this.maybeRemind();
    if (!this.deps.hasPersistentMonitor()) {
      if (this.inFlight || this.timer !== null) {
        this.cancelWarm();
        this.deps.logEvent("cache-warm.stop", { reason: "no persistent monitor" });
      }
      this.inactiveReason = "no persistent monitor";
      return;
    }
    if (this.timer === null && !this.inFlight) this.maybeSchedule();
  }

  /**
   * cache_warming_decision: while we own idle warming, pi stands down. A timer
   * or in-flight warm exists only in the idle phase (agent_start and real
   * requests cancel both), so streaming-phase decisions stay pi's.
   */
  decide(): { action: "stop" } | undefined {
    if (!this.settings().enabled) return undefined;
    if (this.timer === null && !this.inFlight) return undefined;
    this.deps.logEvent("cache-warm.pi-stopped", {});
    return { action: "stop" };
  }

  private maybeRemind(): void {
    const ctx = this.ctx;
    if (!ctx?.hasUI) return;
    const model = ctx.model;
    if (!model) return;
    const tier = retentionTier(this.env());
    const hasTtl = promptCacheTtlMs(model, tier) !== undefined;
    const enabled = this.settings().enabled;
    if (hasTtl && enabled) return;
    const key = `${ctx.sessionManager.getSessionId()}|${model.provider}/${model.id}`;
    if (this.reminded.has(key)) return;
    this.reminded.add(key);
    const steps: string[] = [];
    if (!hasTtl) steps.push(`declare promptCache.${tier} for ${model.provider}/${model.id} in pi's models.json (${PI_PROMPT_CACHE_DOCS})`);
    if (!enabled) steps.push(`set "cacheWarming": { "enabled": true } in ${this.deps.configPath}`);
    ctx.ui.notify(
      `pi-famulus: a persistent monitor may wake this session after the prompt cache has expired. Idle cache warming can keep it warm: ${steps.join(", and ")}.`,
      "info",
    );
  }

  private stopUntilNextRequest(reason: string): void {
    this.cancelWarm();
    this.halted = reason;
    this.inactiveReason = reason;
    this.deps.logEvent("cache-warm.stop", { reason });
  }

  private cancelWarm(): void {
    if (this.timer !== null) this.deps.clock.clearTimeout(this.timer);
    this.timer = null;
    this.nextWarmAt = undefined;
    const inFlight = this.inFlight;
    this.inFlight = null;
    inFlight?.abort();
  }

  private maybeSchedule(): void {
    const settings = this.settings();
    if (!settings.enabled) {
      this.inactiveReason = "cache warming disabled";
      return;
    }
    if (this.agentRunning) return;
    const captured = this.captured;
    if (!captured) {
      this.inactiveReason = this.refusal ?? "no real request yet";
      return;
    }
    if (this.halted) {
      this.inactiveReason = this.halted;
      return;
    }
    if (!this.deps.hasPersistentMonitor()) {
      this.inactiveReason = "no persistent monitor";
      return;
    }
    const now = this.deps.clock.now();
    const anchor = Math.max(this.lastRealAt, this.lastWarmAt);
    const nextAt = anchor + captured.delayMs;
    if (nextAt > this.lastRealAt + settings.maxIdleMs) {
      this.stopUntilNextRequest("maxIdleMs reached");
      return;
    }
    // A refresh later than half the pre-expiry margin is likely a full-price write.
    const deadline = nextAt + Math.floor((captured.ttlMs - captured.delayMs) / 2);
    if (now > deadline) {
      this.stopUntilNextRequest("cache entry likely expired before warming could start");
      return;
    }
    this.cancelWarm();
    this.nextWarmAt = nextAt;
    this.refreshDeadlineAt = deadline;
    this.timer = this.deps.clock.setTimeout(() => {
      this.timer = null;
      void this.fire(captured);
    }, Math.max(0, nextAt - now));
    this.deps.clock.unref?.(this.timer);
  }

  private async fire(captured: Captured): Promise<void> {
    this.nextWarmAt = undefined;
    const ctx = this.ctx;
    if (captured !== this.captured || this.halted) return;
    if (!ctx || this.agentRunning || !ctx.isIdle()) {
      this.inactiveReason = "agent running";
      return;
    }
    if (!this.settings().enabled) {
      this.inactiveReason = "cache warming disabled";
      return;
    }
    if (!this.deps.hasPersistentMonitor()) {
      this.inactiveReason = "no persistent monitor";
      return;
    }
    if (this.deps.clock.now() > this.refreshDeadlineAt) {
      this.stopUntilNextRequest("warm timer fired after the refresh deadline");
      return;
    }
    const controller = new AbortController();
    this.inFlight = controller;
    const sentAt = this.deps.clock.now();
    let refused: string | undefined;
    let message: { stopReason?: string; errorMessage?: string; usage?: { input?: number; output?: number; cacheRead?: number; cacheWrite?: number; cost?: { total?: number } } };
    try {
      const options = {
        maxTokens: 1,
        maxRetries: 0,
        signal: controller.signal,
        sessionId: captured.sessionId,
        cacheRetention: captured.tier,
        // A replay must not replace the conversation's server-side thread (pi >= 1.5).
        disableContinuation: true,
        onPayload: (fresh: unknown) => {
          const built = buildWarmPayload(captured.api, captured.payload, fresh);
          if (!built.ok) {
            refused = built.refused;
            throw new Error(built.refused);
          }
          return built.payload;
        },
      };
      message = await ctx.modelRegistry
        .streamSimple(captured.model, minimalContext(sentAt) as never, options as never)
        .result() as typeof message;
    } catch (error) {
      message = { stopReason: "error", errorMessage: error instanceof Error ? error.message : String(error) };
    }
    if (this.inFlight !== controller) return; // superseded: real request, reset, or monitor stop
    this.inFlight = null;
    if (refused) {
      this.stopUntilNextRequest(`replay refused: ${refused}`);
      return;
    }
    if (message.stopReason === "error" || message.stopReason === "aborted") {
      this.stopUntilNextRequest(`warm failed: ${message.errorMessage ?? message.stopReason}`);
      return;
    }
    const usage = message.usage ?? {};
    const cacheRead = usage.cacheRead ?? 0;
    const cacheWrite = usage.cacheWrite ?? 0;
    const cost = usage.cost?.total ?? 0;
    const hit = cacheRead > 0 && cacheWrite <= cacheRead;
    this.warms++;
    if (hit) this.hits++;
    this.totalCost += cost;
    const record = {
      at: sentAt,
      provider: captured.model.provider,
      model: captured.model.id,
      tier: captured.tier,
      ttlMs: captured.ttlMs,
      hit,
      usage: { input: usage.input ?? 0, output: usage.output ?? 0, cacheRead, cacheWrite },
      cost,
      warms: this.warms,
      totalCost: this.totalCost,
    };
    this.deps.logEvent("cache-warm.sent", record);
    try {
      this.deps.appendEntry(CACHE_WARM_ENTRY_TYPE, record);
    } catch {
      // Accounting is best-effort; the log line above already holds it.
    }
    if (!hit) {
      this.stopUntilNextRequest(`warm missed the cache (cacheRead ${cacheRead}, cacheWrite ${cacheWrite})`);
      return;
    }
    this.lastWarmAt = sentAt;
    this.maybeSchedule();
  }
}

/** Wire the controller to pi's events. Returns it for status and monitor hooks. */
export function registerCacheWarming(pi: ExtensionAPI, deps: CacheWarmDeps): CacheWarmController {
  const controller = new CacheWarmController(deps);
  pi.on("before_provider_request", (event, ctx) => {
    controller.onProviderRequest(event.payload, ctx);
    return undefined;
  });
  pi.on("cache_warming_decision", () => controller.decide());
  pi.on("agent_start", (_event, ctx) => controller.onAgentStart(ctx));
  pi.on("agent_settled", (_event, ctx) => controller.onAgentSettled(ctx));
  pi.on("model_select", () => controller.reset("model changed"));
  pi.on("thinking_level_select", () => controller.reset("thinking level changed"));
  pi.on("session_compact", () => controller.reset("session compacted"));
  pi.on("session_tree", () => controller.reset("session tree navigated"));
  pi.on("session_start", (_event, ctx) => controller.onSessionStart(ctx));
  pi.on("session_shutdown", () => controller.reset("session shutdown"));
  return controller;
}
