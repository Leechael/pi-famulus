/**
 * subagent tool (design doc §4.6).
 *
 * - `tasks`: parallel worker pool (ordinal-preserved results, fail_fast stops
 *   only not-yet-started children);
 * - `chain`: sequential steps with {previous} / {outputs.<label>}
 *   interpolation (unknown labels rejected before anything starts);
 * - sync wait bounded by subagentBudgetMs (default 45000, config subagent
 *   section); on expiry the run continues in the background and completion is
 *   delivered via a <pi-famulus-wake kind="subagent-done"> through the NotifyCenter;
 * - management actions: list / get / status / interrupt / resume / steer.
 *
 * pi-free apart from type-only imports; the registry, runner and session
 * factory are injected.
 */
import { Type } from "typebox";
import type { AgentToolResult, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { VALID_THINKING_LEVELS } from "../thinking-levels";
import { formatSubagentHandover, formatSubagentNotification, truncateTail } from "../format";
import { realClock, type Clock, type ClockTimer } from "../clock";

import type { NotifyCenter } from "../notify";
import { statusGlyph, toolComponent } from "../tui/tool-component";
import type { WorkIndex } from "../work-index";
import { runChain, runTasks, validateChainSteps } from "./pool";
import type { RunRecord, SubagentRegistry } from "./registry";
import type { AgentDefinition, ChildHandle, ChildResult, ChildRunRequest } from "./types";


/** Overall result text cap (§4.6: truncateTail 512 lines / 48KB). */
const RESULT_MAX_LINES = 512;
const RESULT_MAX_BYTES = 48 * 1024;

const MAX_TIMEOUT_MS = 3_600_000;
const MIN_TIMEOUT_MS = 1_000;

// ---------------------------------------------------------------------------
// Schema (§4.6 field-name contract)
// ---------------------------------------------------------------------------

const taskItem = Type.Object({
  agent: Type.Optional(Type.String({ description: "Agent definition name (default: worker)" })),
  prompt: Type.String({ description: "Task prompt for this subagent" }),
  name: Type.Optional(Type.String({ description: "Display name (default: agent name + ordinal)" })),
});

const chainItem = Type.Object({
  agent: Type.Optional(Type.String({ description: "Agent definition name (default: worker)" })),
  prompt: Type.String({
    description: "Prompt for this step; {previous} and {outputs.<label>} interpolate earlier results",
  }),
  label: Type.Optional(Type.String({ description: "Label for referencing this step's output later" })),
});

const subagentParameters = Type.Object({
  tasks: Type.Optional(
    Type.Array(taskItem, { minItems: 1, maxItems: 10, description: "Subagents to run in parallel" }),
  ),
  chain: Type.Optional(
    Type.Array(chainItem, { minItems: 1, description: "Steps to run sequentially (always awaited)" }),
  ),
  async: Type.Optional(
    Type.Boolean({ description: "Return immediately with a run_id; completion arrives via notification" }),
  ),
  concurrency: Type.Optional(
    Type.Number({ minimum: 1, maximum: 8, description: "Max parallel subagents for tasks (default 4)" }),
  ),
  fail_fast: Type.Optional(
    Type.Boolean({
      description: "Cancel not-yet-started subagents on first failure (already-started ones finish)",
    }),
  ),
  model: Type.Optional(
    Type.String({
      description:
        'Model override for all subagents: fuzzy ("haiku"), qualified ("provider/id"), ' +
        `optionally with ":<thinking>" suffix (${VALID_THINKING_LEVELS.join(", ")}; ` +
        'an unknown suffix is dropped with a warning when the base resolves; ' +
        'an unknown base still errors). Default: current model. ' +
        'Use action:"models" to list selectable values.',
    }),
  ),
  timeout_ms: Type.Optional(
    Type.Number({
      description:
        `Time budget per subagent turn in ms (default 1800000, max ${MAX_TIMEOUT_MS}). ` +
        'Passing it does not stop the subagent: you get <pi-famulus-wake kind="subagent-overrun"> ' +
        "and choose extend, steer, or interrupt. With resume: the resumed turn's budget. With extend: the new budget from now.",
      maximum: MAX_TIMEOUT_MS,
    }),
  ),
  action: Type.Optional(
    Type.Union(
      [
        Type.Literal("list"),
        Type.Literal("get"),
        Type.Literal("status"),
        Type.Literal("interrupt"),
        Type.Literal("resume"),
        Type.Literal("steer"),
        Type.Literal("extend"),
        Type.Literal("models"),
      ],
      { description: "Manage an existing run (or list selectable models) instead of starting a new one" },
    ),
  ),
  run_id: Type.Optional(Type.String({ description: "Target run for action" })),
  child_id: Type.Optional(
    Type.String({ description: "Target child (id or name) for steer/interrupt/resume/extend" }),
  ),
  message: Type.Optional(Type.String({ description: "Message content for steer/resume" })),
});

type SubagentParams = {
  tasks?: { agent?: string; prompt: string; name?: string }[];
  chain?: { agent?: string; prompt: string; label?: string }[];
  async?: boolean;
  concurrency?: number;
  fail_fast?: boolean;
  model?: string;
  timeout_ms?: number;
  action?: "list" | "get" | "status" | "interrupt" | "resume" | "steer" | "extend" | "models";
  run_id?: string;
  child_id?: string;
  message?: string;
};

// ---------------------------------------------------------------------------
// Deps
// ---------------------------------------------------------------------------

export interface SubagentToolDeps {
  getRegistry: () => SubagentRegistry | null;
  getNotifyCenter: () => Pick<NotifyCenter, "notify"> | null;
  /** Sync-wait budget in ms (config subagent.budgetMs / subagentBudgetMs). */
  budgetMs: () => number;
  /** Default per-child hard timeout in ms. */
  defaultTimeoutMs: number;
  /** Default tasks worker-pool concurrency. */
  defaultConcurrency: number;
  clock?: Clock;
  /** Live background work; lets a backgrounded run's row redraw as children finish. */
  getIndex?: () => WorkIndex | null;
  /** Agent definition resolver (M5 wires the real loader; default: worker). */
  resolveAgent?: (name: string | undefined) => AgentDefinition;
  /** Selectable models for action:"models" (§4.6); absent → action errors. */
  listModels?: () => {
    provider: string;
    id: string;
    name?: string;
    current: boolean;
    scoped: boolean;
  }[];
}

/** M3 stopgap resolver until M5 wires the real agent loader (§4.8). */
const BUILTIN_WORKER: AgentDefinition = {
  name: "worker",
  description: "General-purpose subagent with the full default tool set",
  tools: ["read", "bash", "edit", "write"],
  systemPrompt: "",
  source: "builtin",
};

function defaultResolveAgent(name: string | undefined): AgentDefinition {
  if (name === undefined || name === BUILTIN_WORKER.name) return BUILTIN_WORKER;
  throw new Error(`unknown agent "${name}" (available: ${BUILTIN_WORKER.name})`);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function errorText(message: string): never {
  throw new Error(message);
}

function buildPrompt(agent: AgentDefinition, prompt: string): string {
  const system = agent.systemPrompt.trim();
  if (!system) return prompt;
  return `${system}\n\n---\n\n${prompt}`;
}

function clampTimeout(timeoutMs: number | undefined, fallback: number): number {
  if (timeoutMs === undefined) return fallback;
  if (!Number.isFinite(timeoutMs)) return fallback;
  return Math.min(MAX_TIMEOUT_MS, Math.max(MIN_TIMEOUT_MS, Math.round(timeoutMs)));
}

function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

function runDurationMs(record: RunRecord, now: number): number {
  const end = record.children.reduce((acc, c) => Math.max(acc, c.endedAt ?? 0), 0);
  return Math.max(0, (end || now) - record.createdAt);
}

/** Per-child sections plus a summary header, capped to 512 lines / 48KB. */
export function formatRunResults(record: RunRecord, now: number = realClock.now()): string {
  const completed = record.children.filter((c) => c.status === "completed").length;
  const header =
    `Run ${record.runId} [${record.kind}] ${record.status} — ` +
    `${completed}/${record.children.length} subagents completed in ${formatDurationMs(runDurationMs(record, now))}.`;
  const sections = record.children.map((child) => {
    const lines = [`## ${child.name} (${child.status})`];
    if (child.model) lines.push(`Model: ${child.model}`);
    if (child.result?.warning) lines.push(`Warning: ${child.result.warning}`);
    if (child.result?.error) lines.push(`Error: ${child.result.error}`);
    if (child.result) lines.push(child.result.text || "(no output)");
    else lines.push("(still running)");
    return lines.join("\n");
  });
  const full = [header, "", ...sections].join("\n\n");
  const t = truncateTail(full, RESULT_MAX_LINES, RESULT_MAX_BYTES);
  return t.truncated
    ? `… (truncated: showing last ${t.text.split("\n").length} of ${t.totalLines} lines)\n${t.text}`
    : full;
}

function toNotificationInfo(record: RunRecord, now: number) {
  return {
    runId: record.runId,
    status: record.status as "completed" | "partial" | "failed" | "interrupted",
    durationMs: runDurationMs(record, now),
    children: record.children.map((c) => ({
      childId: c.childId,
      name: c.name,
      status: c.status,
      text: c.result?.text ?? "",
      error: c.result?.error,
      ...(c.result?.warning !== undefined ? { warning: c.result.warning } : {}),
      ...(c.prompt !== undefined ? { prompt: c.prompt } : {}),
    })),
  };
}

interface RaceOutcome<T> {
  done: boolean;
  value?: T;
}

/** Race a promise against the foreground budget and the tool abort signal. */
function raceBudget<T>(
  promise: Promise<T>,
  budgetMs: number,
  signal: AbortSignal | undefined,
  clock: Clock,
): Promise<RaceOutcome<T>> {
  return new Promise((resolve) => {
    let finished = false;
    let timer: ClockTimer;
    const cleanup = () => {
      clock.clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
    };
    const settle = (outcome: RaceOutcome<T>) => {
      if (finished) return;
      finished = true;
      cleanup();
      resolve(outcome);
    };
    timer = clock.setTimeout(() => settle({ done: false }), budgetMs);
    clock.unref?.(timer);
    const onAbort = () => settle({ done: false });
    if (signal?.aborted) {
      settle({ done: false });
      return;
    }
    signal?.addEventListener("abort", onAbort, { once: true });
    promise.then(
      (value) => settle({ done: true, value }),
      () => settle({ done: false }),
    );
  });
}

// ---------------------------------------------------------------------------
// Tool factory
// ---------------------------------------------------------------------------

export function createSubagentTool(
  deps: SubagentToolDeps,
): ToolDefinition<typeof subagentParameters, unknown> {
  const resolveAgent = deps.resolveAgent ?? defaultResolveAgent;
  const clock = deps.clock ?? realClock;

  const requireRegistry = (): SubagentRegistry => {
    const registry = deps.getRegistry();
    if (!registry) {
      throw new Error("subagent system is not initialized (no active session)");
    }
    return registry;
  };

  /** Last terminal state a subagent-done was sent for, per run. */
  const runDoneSent = new Map<string, string>();
  const notifyRunCompleted = (registry: SubagentRegistry, runId: string): void => {
    const record = registry.get(runId);
    if (!record) return;
    // A resumed child can outlive the run's first-launch completion. The
    // run is not done while it runs; its own settle sends this wake.
    if (record.children.some((c) => c.status === "pending" || c.status === "running")) return;
    // Several observers (first-launch completion, each resumed turn's settle)
    // can see the same all-terminal state. One subagent-done per terminal
    // state, identified by every child's turn number.
    const terminalKey = record.children.map((c) => `${c.childId}:${c.turn ?? 1}`).join("|");
    if (runDoneSent.get(runId) === terminalKey) return;
    runDoneSent.set(runId, terminalKey);
    deps.getNotifyCenter()?.notify(formatSubagentNotification(toNotificationInfo(record, clock.now())));
  };

  /**
   * A child settled while siblings are still in flight. Returns true when a
   * handover was sent. The run-complete notification covers the last child.
   */
  const notifyChildHandover = (
    registry: SubagentRegistry,
    runId: string,
    childId: string,
    handedOver: Set<string>,
  ): boolean => {
    if (handedOver.has(childId)) return false;
    const record = registry.get(runId);
    if (!record) return false;
    const child = record.children.find((c) => c.childId === childId);
    if (!child || child.status === "pending" || child.status === "running") return false;
    const stillRunning = record.children
      .filter((c) => c.status === "pending" || c.status === "running")
      .map((c) => ({ id: c.childId, title: c.name }));
    if (stillRunning.length === 0) return false;
    handedOver.add(childId);
    deps.getNotifyCenter()?.notify(
      formatSubagentHandover({
        runId,
        childId: child.childId,
        name: child.name,
        status: child.status,
        prompt: child.prompt ?? "",
        text: child.result?.text ?? "",
        ...(child.result?.error !== undefined ? { error: child.result.error } : {}),
        ...(child.result?.warning !== undefined ? { warning: child.result.warning } : {}),
        stillRunning,
      }),
    );
    return true;
  };

  const startRun = async (
    params: SubagentParams,
    signal: AbortSignal | undefined,
  ): Promise<AgentToolResult<unknown>> => {
    const registry = requireRegistry();
    const kind = params.tasks !== undefined ? ("tasks" as const) : ("chain" as const);
    const items = (params.tasks ?? params.chain)!;

    if (kind === "chain") validateChainSteps(items);

    const timeoutMs = clampTimeout(params.timeout_ms, deps.defaultTimeoutMs);
    const concurrency = Math.min(
      8,
      Math.max(1, Math.round(params.concurrency ?? deps.defaultConcurrency)),
    );
    const failFast = params.fail_fast === true;

    const resolved = items.map((item, i) => {
      const agent = resolveAgent(item.agent);
      const label = "name" in item ? item.name : undefined;
      const chainLabel = "label" in item ? item.label : undefined;
      const name = label ?? chainLabel ?? `${agent.name}-${i + 1}`;
      return { agent, name, prompt: item.prompt };
    });

    const run = registry.createRun(kind);
    const childIds = resolved.map((r) =>
      registry.addChild(run.runId, { name: r.name, agent: r.agent.name }),
    );

    const makeRequest = (ordinal: number, prompt: string): ChildRunRequest => ({
      childId: childIds[ordinal],
      runId: run.runId,
      name: resolved[ordinal].name,
      prompt: buildPrompt(resolved[ordinal].agent, prompt),
      taskPrompt: prompt,
      agent: resolved[ordinal].agent,
      model: params.model,
      timeoutMs,
      depth: 1,
    });

    const handedOver = new Set<string>();
    let backgrounded = false;
    const armBackground = (): void => {
      backgrounded = true;
      for (const childId of childIds) {
        notifyChildHandover(registry, run.runId, childId, handedOver);
      }
    };
    const watchChild = (ordinal: number, started: Promise<ChildHandle>): Promise<ChildResult> =>
      started.then((handle) => {
        void handle.result.then(() => {
          if (!backgrounded) return;
          notifyChildHandover(registry, run.runId, childIds[ordinal], handedOver);
        });
        return handle.result;
      });

    const completion: Promise<ChildResult[]> =
      kind === "tasks"
        ? runTasks(items, {
            concurrency,
            failFast,
            startChild: (_task, ordinal, ctx) =>
              watchChild(
                ordinal,
                registry.startChild(makeRequest(ordinal, resolved[ordinal].prompt), {
                  shouldStart: () => !ctx.cancelled(),
                }),
              ),
          })
        : runChain(items, {
            startChild: (_step, ordinal, interpolated) =>
              watchChild(ordinal, registry.startChild(makeRequest(ordinal, interpolated))),
          });

    const tracked = completion.then((results) => {
      registry.finalizeRun(
        run.runId,
        kind === "tasks" ? "cancelled (fail_fast)" : "skipped (chain aborted)",
      );
      return results;
    });

    // Completion notification: only when the result was not delivered
    // synchronously (backgrounded or async runs, and aborted waits).
    let deliveredSync = false;
    let settleSync: () => void = () => {};
    const syncSettled = new Promise<void>((resolve) => {
      settleSync = resolve;
    });
    void tracked.then(async () => {
      await syncSettled;
      if (!deliveredSync) notifyRunCompleted(registry, run.runId);
    });

    const backgroundedText = (reason: string): AgentToolResult<unknown> => ({
      content: [
        {
          type: "text",
          text:
            `Started ${items.length} subagent(s) in run ${run.runId}. ${reason}\n` +
            `While others are still running, each finished subagent arrives as <pi-famulus-wake kind="subagent-handover"> ` +
            `with that child's prompt and result. Read it and continue: subagent({action:"resume", run_id, child_id, message}) for that child, ` +
            `or agent_message to steer the ones still running. Do not wait for the whole run. Do not poll. ` +
            `<pi-famulus-wake kind="subagent-done"> arrives when every subagent in the run has finished. ` +
            `Use subagent({action:"get", run_id:"${run.runId}"}) if you need the full record.`,
        },
      ],
      details: { run_id: run.runId, status: "backgrounded" },
    });

    if (params.async === true) {
      armBackground();
      settleSync();
      return backgroundedText("The run is executing in the background.");
    }

    const budgetMs = deps.budgetMs();
    const outcome = await raceBudget(tracked, budgetMs, signal, clock);
    if (outcome.done) {
      deliveredSync = true;
      settleSync();
      const record = registry.get(run.runId);
      const text = record
        ? formatRunResults(record)
        : `Run ${run.runId} finished but its record is gone.`;
      return {
        content: [{ type: "text", text }],
        details: { run_id: run.runId, status: record?.status ?? "completed", results: outcome.value },
      };
    }
    armBackground();
    settleSync();
    return backgroundedText(
      `The foreground budget (${Math.round(budgetMs / 1000)}s) elapsed and the run continues in the background.`,
    );
  };

  // -------------------------------------------------------------------------
  // Management actions
  // ---------------------------------------------------------------------------

  const requireRun = (registry: SubagentRegistry, runId: string | undefined): RunRecord => {
    if (!runId) throw new Error(`run_id is required for this action`);
    const record = registry.get(runId);
    if (!record) {
      const known = registry
        .list()
        .map((r) => r.runId)
        .join(", ");
      throw new Error(`unknown run_id "${runId}" (known runs: ${known || "none"})`);
    }
    return record;
  };

  const runAction = async (params: SubagentParams): Promise<AgentToolResult<unknown>> => {
    const action = params.action!;

    // "models" needs neither a registry nor a run: list selectable models.
    if (action === "models") {
      if (!deps.listModels) {
        return errorText('action "models" is not available in this session');
      }
      const models = deps.listModels();
      if (models.length === 0) {
        return { content: [{ type: "text", text: "No models available." }], details: { models: [] } };
      }
      const scoped = models.some((m) => m.scoped);
      const lines = models.map((m) => {
        const marks = [m.current ? "current" : "", m.scoped ? "scoped" : ""]
          .filter(Boolean)
          .join(", ");
        return `${m.provider}/${m.id}${m.name ? ` — ${m.name}` : ""}${marks ? ` (${marks})` : ""}`;
      });
      const header = scoped
        ? `${models.length} selectable model(s) (whitelist via enabledModels/--models is active):`
        : `${models.length} selectable model(s):`;
      return {
        content: [{ type: "text", text: `${header}\n${lines.join("\n")}` }],
        details: { models },
      };
    }

    const registry = requireRegistry();

    if (action === "list") {
      const runs = registry.list();
      if (runs.length === 0) {
        return { content: [{ type: "text", text: "No subagent runs in this session." }], details: { runs: [] } };
      }
      const now = clock.now();
      const lines = runs.map((run) => {
        const counts = new Map<string, number>();
        for (const c of run.children) counts.set(c.status, (counts.get(c.status) ?? 0) + 1);
        const summary = [...counts.entries()].map(([s, n]) => `${n} ${s}`).join(", ");
        const ago = formatDurationMs(now - run.createdAt);
        return `${run.runId} [${run.kind}] ${run.status} — ${run.children.length} children (${summary}), created ${ago} ago`;
      });
      return {
        content: [{ type: "text", text: `${runs.length} run(s):\n${lines.join("\n")}` }],
        details: { runs },
      };
    }

    const record = requireRun(registry, params.run_id);

    if (action === "get") {
      return {
        content: [{ type: "text", text: formatRunResults(record, clock.now()) }],
        details: { run_id: record.runId, status: record.status },
      };
    }

    if (action === "status") {
      const now = clock.now();
      const lines = record.children.map((child) => {
        const elapsed = formatDurationMs((child.endedAt ?? now) - child.startedAt);
        let line = `${child.name} (${child.childId}): ${child.status}, ${elapsed} elapsed`;
        if (child.model) line += `, model ${child.model}`;
        if (child.status === "running" || child.status === "pending") {
          const last = registry.handle(child.childId)?.lastEventAt() ?? child.startedAt;
          line += `, last event ${formatDurationMs(now - last)} ago`;
        }
        if (child.result?.error) line += `, error: ${child.result.error}`;
        return line;
      });
      const header = `Run ${record.runId} [${record.kind}] ${record.status}:`;
      return {
        content: [{ type: "text", text: [header, ...lines].join("\n") }],
        details: { run_id: record.runId, status: record.status },
      };
    }

    if (action === "interrupt") {
      const targets = record.children.filter((c) => {
        if (params.child_id && c.childId !== params.child_id && c.name !== params.child_id) return false;
        return c.status === "running" || c.status === "pending";
      });
      if (params.child_id && !record.children.some((c) => c.childId === params.child_id || c.name === params.child_id)) {
        const known = record.children.map((c) => `${c.name} (${c.childId})`).join(", ");
        throw new Error(`no child "${params.child_id}" in run ${record.runId} (children: ${known})`);
      }
      for (const child of targets) {
        await registry.handle(child.childId)?.interrupt();
      }
      const scope = params.child_id ? `subagent ${params.child_id}` : `${targets.length} subagent(s)`;
      return {
        content: [{ type: "text", text: `Interrupted ${scope} in run ${record.runId}.` }],
        details: { run_id: record.runId, interrupted: targets.map((c) => c.childId) },
      };
    }

    if (action === "steer") {
      if (!params.message) throw new Error("message is required for steer");
      const handle = resolveSingleActiveChild(registry, record, params.child_id, "steer");
      await handle.steer(params.message);
      return {
        content: [{ type: "text", text: `Steered subagent in run ${record.runId}: delivered "${params.message}".` }],
        details: { run_id: record.runId },
      };
    }

    if (action === "extend") {
      const handle = resolveSingleActiveChild(registry, record, params.child_id, "extend");
      const child = record.children.find((c) => c.childId === handle.childId);
      const ms = params.timeout_ms === undefined ? undefined : clampTimeout(params.timeout_ms, deps.defaultTimeoutMs);
      const { deadlineAt, hardDeadlineAt } = handle.extend(ms);
      const now = clock.now();
      const hard =
        hardDeadlineAt === null
          ? ""
          : ` The configured hard ceiling (hardTimeoutMs) is not moved: it stops this subagent in ${formatDurationMs(Math.max(0, hardDeadlineAt - now))}.`;
      return {
        content: [
          {
            type: "text",
            text:
              `Extended subagent ${child?.name ?? handle.childId} (${handle.childId}) in run ${record.runId}: ` +
              `the next <pi-famulus-wake kind="subagent-overrun"> comes in ${formatDurationMs(deadlineAt - now)} if it is still running. ` +
              `Its result arrives as a wake when it finishes; do not poll.${hard}`,
          },
        ],
        details: { run_id: record.runId, child_id: handle.childId, deadline_at: deadlineAt, hard_deadline_at: hardDeadlineAt },
      };
    }

    // resume
    if (!params.message) throw new Error("message is required for resume");
    const child = resolveSingleTerminalChild(record, params.child_id);
    // A resume is a new turn with its own soft budget: timeout_ms when given,
    // else the child's spawn budget (the runner's default). Returns once the
    // request is accepted; it never waits for an admission slot.
    const { queuedBehind } = await registry.resumeChild(
      child.childId,
      params.message,
      params.timeout_ms === undefined ? {} : { timeoutMs: clampTimeout(params.timeout_ms, deps.defaultTimeoutMs) },
    );
    // Resume is asynchronous. If siblings are still running, hand this child
    // back as soon as it finishes; otherwise the run-complete wake covers it.
    const handedOver = new Set<string>();
    void registry.getResult(child.childId)?.then(() => {
      if (!notifyChildHandover(registry, record.runId, child.childId, handedOver)) {
        notifyRunCompleted(registry, record.runId);
      }
    });
    return {
      content: [
        {
          type: "text",
          text:
            `Resumed subagent ${child.name} (${child.childId}) in run ${record.runId}. ` +
            queuedText(queuedBehind) +
            "You will be notified via <pi-famulus-wake kind=\"subagent-handover\"> if others are still running, " +
            "otherwise via <pi-famulus-wake kind=\"subagent-done\"> when it completes. Do not poll.",
        },
      ],
      details: { run_id: record.runId, child_id: child.childId, queued_behind: queuedBehind },
    };
  };

  return {
    name: "subagent",
    label: "Subagent",
    description:
      "Run subagents in parallel (tasks) or sequentially (chain with {previous}/{outputs.<label>} " +
      "interpolation). By default the call waits up to a foreground budget (default 45s); longer runs " +
      "continue in the background. Each child that finishes while others are still running wakes you with " +
      "<pi-famulus-wake kind=\"subagent-handover\"> (its prompt and result). The whole run wakes you with <pi-famulus-wake kind=\"subagent-done\">. " +
      "Never poll or sleep to wait. " +
      "A subagent still running past its timeout_ms is not stopped; it wakes you with " +
      "<pi-famulus-wake kind=\"subagent-overrun\"> so you can extend, steer, or interrupt it. " +
      "Use action=list/get/status/interrupt/resume/steer/extend to manage existing runs.",
    promptSnippet: "Fan out subagents in parallel or sequence them in a chain",
    promptGuidelines: [
      'When a <pi-famulus-wake kind="subagent-handover"> arrives, read <prompt> and <result> immediately and continue: subagent({action:"resume", run_id, child_id, message}) for that child, or agent_message to steer children that are still running. Do not wait for the rest of the run.',
      "Subagent runs that exceed the foreground budget continue in the background; you are notified per finished child and again when the run completes — do not poll.",
      "A failed subagent does not fail the whole run; inspect per-subagent sections in the result.",
      "<pi-famulus-wake> is a system wake, not a user reply. kind=subagent-handover is one child; kind=subagent-done is the whole run.",
    ],
    parameters: subagentParameters,
    renderResult(result, { expanded }, theme, context) {
      const details = result.details as { run_id?: string; status?: string } | undefined;
      const record = details?.run_id ? deps.getRegistry()?.get(details.run_id) : undefined;
      if (details?.status === "backgrounded" && record) {
        // UI row only: one line per child with live status. The model-facing
        // instructions stay in result.content.
        const index = deps.getIndex?.() ?? null;
        const state = context.state as { unsubscribe?: () => void };
        const active = record.children.some((c) => c.status === "pending" || c.status === "running");
        if (index && !state.unsubscribe && active) {
          state.unsubscribe = index.onChange(() => {
            const now = deps.getRegistry()?.get(record.runId);
            if (!now || now.children.every((c) => c.status !== "pending" && c.status !== "running")) {
              state.unsubscribe?.();
              state.unsubscribe = () => {};
            }
            context.invalidate();
          });
        }
        const now = clock.now();
        const lines = [theme.fg("muted", `run ${record.runId} · ${record.status} · /tasks`)];
        for (const c of record.children) {
          const g = statusGlyph(c.status);
          const age = formatDurationMs((c.endedAt ?? now) - c.startedAt);
          const err = c.result?.error ? ` · ${c.result.error}` : "";
          lines.push(`  ${theme.fg(g.color as never, g.glyph)} ${c.name} ${theme.fg("dim", `${c.status} ${age}${err}`)}`);
        }
        return toolComponent(lines) as never;
      }
      const text = result.content
        .filter((c): c is { type: "text"; text: string } => c.type === "text")
        .map((c) => c.text)
        .join("\n");
      const all = text.split("\n");
      const shown = expanded ? all : all.slice(0, 10);
      const out = shown.map((l) => theme.fg("toolOutput", l));
      if (shown.length < all.length) out.push(theme.fg("muted", `... (${all.length - shown.length} more lines, ctrl+o to expand)`));
      return toolComponent(out) as never;
    },
    async execute(_toolCallId, rawParams, signal, _onUpdate, _ctx) {
      const params = rawParams as SubagentParams;
      const hasTasks = params.tasks !== undefined;
      const hasChain = params.chain !== undefined;
      const hasAction = params.action !== undefined;
      if (hasAction && (hasTasks || hasChain)) {
        return errorText("action is mutually exclusive with tasks/chain");
      }
      if (hasTasks && hasChain) {
        return errorText("tasks and chain are mutually exclusive");
      }
      if (!hasAction && !hasTasks && !hasChain) {
        return errorText("one of tasks, chain, or action is required");
      }
      if (hasAction) return runAction(params);
      return startRun(params, signal);
    },
  };
}

/** Resume result clause for a turn waiting for an admission slot ("" when it started at once). */
function queuedText(queuedBehind: number | null): string {
  if (queuedBehind === null) return "";
  const ahead = queuedBehind === 0 ? "" : ` and ${queuedBehind} subagent(s) are waiting ahead of it`;
  return `Every subagent slot is busy${ahead}, so it is queued and starts when a slot frees. `;
}

function resolveSingleActiveChild(
  registry: SubagentRegistry,
  record: RunRecord,
  childIdOrName: string | undefined,
  what: string,
) {
  if (childIdOrName) {
    const handle = registry.findChild(record.runId, childIdOrName);
    if (!handle) {
      const known = record.children.map((c) => `${c.name} (${c.childId})`).join(", ");
      throw new Error(`no child "${childIdOrName}" in run ${record.runId} (children: ${known})`);
    }
    return handle;
  }
  const active = record.children.filter((c) => c.status === "running");
  if (active.length === 1) {
    const handle = registry.handle(active[0].childId);
    if (handle) return handle;
  }
  throw new Error(
    `cannot ${what}: run ${record.runId} has ${active.length} running subagents; specify child_id ` +
      `(${record.children.map((c) => `${c.name} (${c.childId}): ${c.status}`).join(", ") || "none"})`,
  );
}

function resolveSingleTerminalChild(record: RunRecord, childIdOrName: string | undefined) {
  const matches = (c: RunRecord["children"][number]) =>
    c.status === "completed" || c.status === "failed" || c.status === "interrupted";
  if (childIdOrName) {
    const child = record.children.find((c) => c.childId === childIdOrName || c.name === childIdOrName);
    if (!child) {
      const known = record.children.map((c) => `${c.name} (${c.childId})`).join(", ");
      throw new Error(`no child "${childIdOrName}" in run ${record.runId} (children: ${known})`);
    }
    if (child.status === "pending") {
      // Queued for an admission slot (a launch or an earlier resume): steer
      // would fail too, and a second resume adds nothing.
      throw new Error(
        `subagent ${child.name} (${child.childId}) is already queued and starts when a subagent slot frees; ` +
          "its result arrives as a wake when it finishes. Do not resume it again.",
      );
    }
    if (!matches(child)) {
      throw new Error(`subagent ${child.name} (${child.childId}) is still ${child.status}; use steer instead`);
    }
    return child;
  }
  const terminal = record.children.filter(matches);
  if (terminal.length === 1 && record.children.length === 1) return terminal[0];
  throw new Error(
    `resume requires child_id (children: ${record.children.map((c) => `${c.name} (${c.childId}): ${c.status}`).join(", ") || "none"})`,
  );
}
