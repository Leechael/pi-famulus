/**
 * Optional decision-model layer for monitor wakes: SHADOW MODE ONLY.
 *
 * A cheap pi classifier model judges whether an admitted monitor output batch
 * needs the agent. Its answer is only recorded (JSONL); it never suppresses,
 * delays or changes a wake. Off by default (`decisionModel.enabled`).
 *
 * Reuses pi's model registry and credentials (`ctx.modelRegistry`); there is
 * no famulus-side API key. When the flag is off, no classifier is available,
 * or the running pi lacks classifier support, nothing runs and nothing is
 * surfaced to the model.
 */
import { appendFile, mkdir, rename, stat } from "node:fs/promises";
import { dirname, join } from "node:path";
import { realClock, type Clock, type ClockTimer } from "./clock";

/** Resolved `decisionModel` section of config.json. */
export interface DecisionModelConfig {
  enabled: boolean;
  /** "provider/id"; unset = first available classifier model. */
  model?: string;
  timeoutMs: number;
}

export const DEFAULT_DECISION_MODEL_CONFIG: DecisionModelConfig = { enabled: false, timeoutMs: 3000 };
export const DECISION_TIMEOUT_MIN_MS = 100;
export const DECISION_TIMEOUT_MAX_MS = 60_000;

/** Bounds on what is sent to the classifier provider per batch. */
export const DECISION_MAX_LINES = 40;
export const DECISION_MAX_CHARS = 4000;
export const DECISION_MAX_FIELD_CHARS = 500;
export const DECISION_MAX_PREVIOUS_CHARS = 500;
/** Classifications in flight at once; extra batches are recorded as skipped. */
export const DECISION_MAX_IN_FLIGHT = 4;
/** Shadow log rotates to `<file>.1` past this size. */
export const DECISION_LOG_MAX_BYTES = 10 * 1024 * 1024;
export const DECISION_LOG_FILE = "decision-shadow.jsonl";
/** A batch whose needs_attention probability is below this would have been suppressed. */
export const DECISION_SUPPRESS_BELOW = 0.5;

export function decisionLogPath(home: string): string {
  return join(home, DECISION_LOG_FILE);
}

/** Parse the raw `decisionModel` config value. Anything malformed stays off. */
export function parseDecisionModelConfig(raw: unknown): DecisionModelConfig | undefined {
  if (typeof raw !== "object" || raw === null) return undefined;
  const obj = raw as Record<string, unknown>;
  const config: DecisionModelConfig = { ...DEFAULT_DECISION_MODEL_CONFIG, enabled: obj.enabled === true };
  if (typeof obj.model === "string" && obj.model.includes("/") && obj.model.trim().length > 2) {
    config.model = obj.model.trim();
  }
  if (typeof obj.timeoutMs === "number" && Number.isFinite(obj.timeoutMs)) {
    config.timeoutMs = Math.min(DECISION_TIMEOUT_MAX_MS, Math.max(DECISION_TIMEOUT_MIN_MS, Math.floor(obj.timeoutMs)));
  }
  return config;
}

// Minimal structural surface of pi's classifier API (pi-ai ClassifierModel,
// ClassifierContext, ClassifierResult; coding-agent ModelRegistry). Typed
// locally and feature-detected so an older pi without classify is a no-op.
export interface ClassifierModelLike {
  provider: string;
  id: string;
  type?: string;
}

export interface ClassifierResultLike {
  provider?: string;
  model?: string;
  answers?: Record<string, { type?: string; probability?: number } | undefined>;
  usage?: { input?: number; output?: number; totalTokens?: number; cost?: { total?: number } };
  stopReason?: string;
  errorMessage?: string;
}

export type JsonValueLike = string | number | boolean | null | JsonValueLike[] | { [key: string]: JsonValueLike };
export type ClassifierStateLike = { [key: string]: JsonValueLike };
export interface ClassifierBoolQuestionLike {
  type: "bool";
  instructions: string;
  criteria: { true: string; false: string };
}
export interface ClassifierContextLike {
  state: ClassifierStateLike;
  questions: Record<string, ClassifierBoolQuestionLike>;
}

export interface ClassifierRegistryLike {
  getAvailableOfType?: (type: "classifier") => Promise<readonly ClassifierModelLike[]>;
  getModelOfType?: (type: "classifier", provider: string, modelId: string) => ClassifierModelLike | undefined;
  classify?: (
    model: never,
    context: ClassifierContextLike,
    options?: { signal?: AbortSignal },
  ) => Promise<ClassifierResultLike>;
}

export interface DecisionInput {
  taskId: string;
  description: string;
  command: string;
  /** The batch text as admitted for the wake. */
  text: string;
  /** "batch" = ordinary output lines; "summary" = overload summary. */
  kind: "batch" | "summary";
}

export interface DecisionRecord {
  v: 1;
  ts: number;
  sessionId: string;
  taskId: string;
  kind: "batch" | "summary";
  /** Lines in the admitted batch (before bounding). */
  lines: number;
  /** Characters sent as state (after bounding). */
  stateChars: number;
  truncated: boolean;
  model: string | null;
  needsAttention: boolean | null;
  probability: number | null;
  latencyMs: number | null;
  usage: { input?: number; output?: number; costTotal?: number } | null;
  stopReason: string | null;
  error: string | null;
}

export const NEEDS_ATTENTION_QUESTION_KEY = "needs_attention";

/** Deterministic, short question. The description is the watcher's stated intent. */
export function buildQuestion(description: string): Record<string, ClassifierBoolQuestionLike> {
  return {
    [NEEDS_ATTENTION_QUESTION_KEY]: {
      type: "bool",
      instructions: `Does this monitor output show a problem or event the agent watching '${clip(description, DECISION_MAX_FIELD_CHARS)}' must act on now?`,
      criteria: {
        true: "The output reports an error, failure, completion, or the specific event the description asks to watch for.",
        false: "The output is routine progress, noise, or repetition that the agent can safely ignore for now.",
      },
    },
  };
}

/** Bounded classifier state: last DECISION_MAX_LINES lines, at most DECISION_MAX_CHARS. */
export function buildState(input: DecisionInput, previous?: string): { state: ClassifierStateLike; lines: number; stateChars: number; truncated: boolean } {
  const all = input.text.split("\n");
  const lines = all.length;
  const kept = all.slice(-DECISION_MAX_LINES);
  let truncated = kept.length < all.length;
  let output = kept.join("\n");
  if (output.length > DECISION_MAX_CHARS) {
    output = output.slice(output.length - DECISION_MAX_CHARS);
    truncated = true;
  }
  const state: ClassifierStateLike = {
    monitor_description: clip(input.description, DECISION_MAX_FIELD_CHARS),
    command: clip(input.command, DECISION_MAX_FIELD_CHARS),
    output_kind: input.kind,
    output_line_count: lines,
    output_truncated: truncated,
    output,
  };
  if (previous) state.previous_output = previous.slice(-DECISION_MAX_PREVIOUS_CHARS);
  const stateChars = String(state.monitor_description).length + String(state.command).length + output.length + (previous ? Math.min(previous.length, DECISION_MAX_PREVIOUS_CHARS) : 0);
  return { state, lines, stateChars, truncated };
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

export interface DecisionShadowOptions {
  config: DecisionModelConfig;
  getRegistry: () => unknown;
  sessionId: () => string;
  /** JSONL destination. */
  logPath: string;
  clock?: Clock;
  /** Test seam; default appends to logPath with rotation. */
  append?: (line: string) => Promise<void> | void;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
}

type Resolution = { model: ClassifierModelLike; classify: NonNullable<ClassifierRegistryLike["classify"]> } | { unavailable: string };

export interface DecisionShadowStats {
  decisions: number;
  wouldSuppress: number;
  errors: number;
  skipped: number;
  model: string | null;
  unavailable: string | null;
}

export class DecisionShadow {
  private readonly opts: DecisionShadowOptions;
  private readonly clock: Clock;
  private readonly resolutions = new WeakMap<object, Promise<Resolution>>();
  private readonly inFlight = new Set<AbortController>();
  private readonly previous = new Map<string, string>();
  private writes: Promise<void> = Promise.resolve();
  private disposed = false;
  private readonly counts: DecisionShadowStats = { decisions: 0, wouldSuppress: 0, errors: 0, skipped: 0, model: null, unavailable: null };
  private unavailableLogged = false;

  constructor(opts: DecisionShadowOptions) {
    this.opts = opts;
    this.clock = opts.clock ?? realClock;
  }

  get enabled(): boolean {
    return this.opts.config.enabled;
  }

  stats(): DecisionShadowStats {
    return { ...this.counts };
  }

  /**
   * Record a shadow judgement for an admitted wake. Returns at once: the
   * classification runs detached and never touches wake delivery.
   */
  observe(input: DecisionInput): void {
    if (this.disposed || !this.opts.config.enabled) return;
    try {
      const registry = this.opts.getRegistry();
      if (!registry || typeof registry !== "object") return;
      const previous = this.previous.get(input.taskId);
      const built = buildState(input, previous);
      this.previous.set(input.taskId, input.text.slice(-DECISION_MAX_PREVIOUS_CHARS));
      void this.run(registry as ClassifierRegistryLike & object, input, built).catch((error: unknown) => {
        this.opts.logEvent?.("decision.error", { id: input.taskId, error: String(error) });
      });
    } catch (error) {
      this.opts.logEvent?.("decision.error", { id: input.taskId, error: String(error) });
    }
  }

  /** A monitor ended: forget its previous-output context. */
  forget(taskId: string): void {
    this.previous.delete(taskId);
  }

  /** Abort in-flight classifications. Later results are not recorded. */
  dispose(): void {
    this.disposed = true;
    for (const controller of this.inFlight) controller.abort();
    this.inFlight.clear();
    this.previous.clear();
  }

  /** Resolves once queued log writes are flushed (tests, shutdown). */
  flushed(): Promise<void> {
    return this.writes;
  }

  private resolve(registry: ClassifierRegistryLike & object): Promise<Resolution> {
    let pending = this.resolutions.get(registry);
    if (!pending) {
      pending = this.resolveUncached(registry).catch((error: unknown) => ({ unavailable: `resolution failed: ${String(error)}` }));
      this.resolutions.set(registry, pending);
    }
    return pending;
  }

  private async resolveUncached(registry: ClassifierRegistryLike): Promise<Resolution> {
    const classify = registry.classify;
    if (typeof classify !== "function") return { unavailable: "classify unsupported by this pi" };
    const bound = classify.bind(registry) as NonNullable<ClassifierRegistryLike["classify"]>;
    const configured = this.opts.config.model;
    if (configured) {
      if (typeof registry.getModelOfType !== "function") return { unavailable: "classifier models unsupported by this pi" };
      const slash = configured.indexOf("/");
      const model = registry.getModelOfType("classifier", configured.slice(0, slash), configured.slice(slash + 1));
      if (!model) return { unavailable: `configured classifier ${configured} not found` };
      return { model, classify: bound };
    }
    if (typeof registry.getAvailableOfType !== "function") return { unavailable: "classifier models unsupported by this pi" };
    const available = await registry.getAvailableOfType("classifier");
    const model = available[0];
    if (!model) return { unavailable: "no classifier model with credentials" };
    return { model, classify: bound };
  }

  private async run(
    registry: ClassifierRegistryLike & object,
    input: DecisionInput,
    built: ReturnType<typeof buildState>,
  ): Promise<void> {
    const resolution = await this.resolve(registry);
    if (this.disposed) return;
    if ("unavailable" in resolution) {
      this.counts.unavailable = resolution.unavailable;
      if (!this.unavailableLogged) {
        this.unavailableLogged = true;
        this.opts.logEvent?.("decision.unavailable", { reason: resolution.unavailable });
      }
      return;
    }
    const modelName = `${resolution.model.provider}/${resolution.model.id}`;
    this.counts.model = modelName;
    this.counts.unavailable = null;
    const base = {
      v: 1 as const,
      sessionId: this.opts.sessionId(),
      taskId: input.taskId,
      kind: input.kind,
      lines: built.lines,
      stateChars: built.stateChars,
      truncated: built.truncated,
      model: modelName,
    };
    if (this.inFlight.size >= DECISION_MAX_IN_FLIGHT) {
      this.counts.skipped += 1;
      this.write({ ...base, ts: this.clock.now(), needsAttention: null, probability: null, latencyMs: null, usage: null, stopReason: null, error: "skipped: in-flight limit" });
      return;
    }
    const controller = new AbortController();
    this.inFlight.add(controller);
    const startedAt = this.clock.now();
    let timer: ClockTimer | null = null;
    const timedOut = new Promise<"timeout">((resolveTimeout) => {
      timer = this.clock.setTimeout(() => resolveTimeout("timeout"), this.opts.config.timeoutMs);
      this.clock.unref?.(timer);
    });
    let outcome: ClassifierResultLike | "timeout" | { thrown: unknown };
    try {
      outcome = await Promise.race([
        // classify "never rejects" in pi; a sync throw or rejection is still recorded.
        Promise.resolve()
          .then(() => resolution.classify(resolution.model as never, { state: built.state, questions: buildQuestion(input.description) }, { signal: controller.signal }))
          .catch((error: unknown) => ({ thrown: error })),
        timedOut,
      ]);
    } finally {
      if (timer !== null) this.clock.clearTimeout(timer);
      this.inFlight.delete(controller);
    }
    if (this.disposed) return;
    const latencyMs = this.clock.now() - startedAt;
    if (outcome === "timeout") {
      controller.abort();
      this.counts.errors += 1;
      this.write({ ...base, ts: this.clock.now(), needsAttention: null, probability: null, latencyMs, usage: null, stopReason: "aborted", error: `timeout after ${this.opts.config.timeoutMs}ms` });
      return;
    }
    if ("thrown" in outcome) {
      this.counts.errors += 1;
      this.write({ ...base, ts: this.clock.now(), needsAttention: null, probability: null, latencyMs, usage: null, stopReason: "error", error: String(outcome.thrown) });
      return;
    }
    const answer = outcome.answers?.[NEEDS_ATTENTION_QUESTION_KEY];
    const probability = typeof answer?.probability === "number" && Number.isFinite(answer.probability) ? answer.probability : null;
    const usage = outcome.usage
      ? { input: outcome.usage.input, output: outcome.usage.output, costTotal: outcome.usage.cost?.total }
      : null;
    const failed = outcome.stopReason !== undefined && outcome.stopReason !== "stop";
    const error = failed ? outcome.errorMessage ?? outcome.stopReason ?? "error" : probability === null ? "no needs_attention answer" : null;
    if (error) this.counts.errors += 1;
    else {
      this.counts.decisions += 1;
      if ((probability as number) < DECISION_SUPPRESS_BELOW) this.counts.wouldSuppress += 1;
    }
    this.write({
      ...base,
      model: outcome.provider && outcome.model ? `${outcome.provider}/${outcome.model}` : modelName,
      ts: this.clock.now(),
      needsAttention: error ? null : (probability as number) >= DECISION_SUPPRESS_BELOW,
      probability: error ? null : probability,
      latencyMs,
      usage,
      stopReason: outcome.stopReason ?? null,
      error,
    });
  }

  private write(record: DecisionRecord): void {
    const line = `${JSON.stringify(record)}\n`;
    const append = this.opts.append ?? ((text: string) => appendRotating(this.opts.logPath, text));
    this.writes = this.writes
      .then(() => append(line))
      .catch((error: unknown) => {
        this.opts.logEvent?.("decision.log-error", { error: String(error) });
      });
  }
}

async function appendRotating(path: string, text: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const size = await stat(path).then((s) => s.size, () => 0);
  if (size + text.length > DECISION_LOG_MAX_BYTES) await rename(path, `${path}.1`).catch(() => {});
  await appendFile(path, text, { mode: 0o600 });
}

/** One status line for /famulus. */
export function formatDecisionShadow(shadow: DecisionShadow | null, logPath: string): string {
  if (!shadow?.enabled) return "off";
  const s = shadow.stats();
  if (s.unavailable && !s.model) return `shadow · unavailable (${s.unavailable}) · ${logPath}`;
  const judged = s.decisions;
  const extra = [s.errors ? `${s.errors} errors` : "", s.skipped ? `${s.skipped} skipped` : ""].filter(Boolean).join(", ");
  return `shadow · ${s.model ?? "model pending"} · would suppress ${s.wouldSuppress} of ${judged}${extra ? ` (${extra})` : ""} · ${logPath}`;
}
