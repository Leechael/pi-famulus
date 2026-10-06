/**
 * monitor tool (design doc §4.4).
 *
 * Starts a long-lived `kind:"monitor"` process via pi-famulus, watches its
 * output stream, and injects line batches as <pi-famulus-wake kind="monitor"> messages.
 * Batching (LineBatcher) and throttling (RateLimiter) happen extension-side;
 * a monitor is stopped when at least half its batches are dropped in a rolling
 * 30-second window.
 */
import { Type } from "typebox";
import type { ExtensionContext, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { formatMonitorEvent } from "./format";
import { MONITOR_IDLE_INSTRUCTION } from "./behavior-guidelines";
import { realClock, type Clock, type ClockTimer } from "./clock";

import type { ManagerClient, ManagerEvent, TaskRecord } from "./manager-client";
import { LineBatcher, RateLimiter, SaturationWindow } from "./monitor-batching";
import type { NotifyCenter } from "./notify";
import { statusGlyph, toolComponent } from "./tui/tool-component";


const DEFAULT_TIMEOUT_MS = 300_000;
const MIN_TIMEOUT_MS = 1_000;
const MAX_TIMEOUT_MS = 3_600_000;
/** Rolling-window drop ratio required before a monitor is auto-stopped. */
const SATURATION_WINDOW_MS = 30_000;
const SATURATION_DROP_RATIO = 0.5;
const SATURATION_MIN_BATCHES = 10;
/**
 * A clean exit this soon after an event was caused by it (`grep -m1`). Its
 * notice is appended without starting a turn: as a wake it cost every such monitor a
 * turn spent acknowledging "exited" (eval batch 1, 2026-09-29).
 */
const EXIT_AFTER_EVENT_MS = 2_000;

/**
 * Model-facing, after the start line. The transcript row shows the first line only.
 * After Claude Code's monitor start result: say everything that will arrive,
 * so there is nothing left to verify, and name the polls. With "do not check
 * on it", models still called task_list once while "waiting" (eval
 * 2026-09-30c: 5 of the 6 FAILs outside grok). Its "Keep working" was tried
 * and dropped: gpt-6-luna started duplicate monitors (eval 2026-09-30d).
 */
export const MONITOR_STARTED_INSTRUCTION =
  'You will get a <pi-famulus-wake kind="monitor"> for each event, and a notice when it exits or times out. ' +
  "Do not poll it (task_list, task_output, or reading what it watches) or sleep. " +
  MONITOR_IDLE_INSTRUCTION;
/**
 * Events for ids the registry does not know yet. The manager streams a monitor
 * from spawn, so output and even the exit can arrive before `start()` has the
 * task id (same socket read as the start response). Bounded: unrelated shell
 * exits land here too and age out.
 */
const EARLY_MAX_IDS = 32;
const EARLY_MAX_CHARS = 64 * 1024;

export interface MonitorDeps {
  getClient: () => ManagerClient | null;
  sessionEnv: (ctx: ExtensionContext) => Record<string, string>;
  getNotifyCenter: () => NotifyCenter | null;
  trackTask: (taskId: string, meta: { kind: string; command: string; cwd?: string }) => void;
  /** Optional TUI toast for lifecycle notices (exit / timeout / rate-limit). */
  toast?: (message: string, type?: "info" | "warning" | "error") => void;
  clock?: Clock;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
  /** A known monitor ended (event, replayed early exit, or reconcile). */
  onExited?: (taskId: string, event: ManagerEvent) => void;
  /**
   * After a timeout or rate-limit stop. The process may already have exited
   * with its exit event lost, in which case no further event will settle it.
   */
  afterStop?: () => void;
}

interface MonitorEntry {
  taskId: string;
  description: string;
  startedAt: number;
  batcher: LineBatcher;
  limiter: RateLimiter;
  saturation: SaturationWindow;
  droppedLinesPending: number;
  timeoutTimer: ClockTimer | null;
  stopped: boolean;
  cursor: number;
  recovering: boolean;
  queuedOutput: { chunk: string; cursor?: number }[];
  lastEventAt?: number;
  /** The model asked to stop it (task_stop): what is left is not news. */
  stopRequested?: boolean;
}

export class MonitorRegistry {
  private readonly deps: MonitorDeps;
  private readonly clock: Clock;
  private readonly entries = new Map<string, MonitorEntry>();
  private readonly changeListeners = new Set<() => void>();
  private readonly early = new Map<string, { chunks: { chunk: string; cursor?: number }[]; chars: number; exit?: ManagerEvent }>();

  constructor(deps: MonitorDeps) {
    this.deps = deps;
    this.clock = deps.clock ?? realClock;
  }

  /** Subscribe to start/stop transitions (fleet status refresh). */
  onChange(cb: () => void): () => void {
    this.changeListeners.add(cb);
    return () => this.changeListeners.delete(cb);
  }

  /** Active monitors for the fleet status surface. */
  listActive(): { taskId: string; description: string; startedAt: number }[] {
    return [...this.entries.values()]
      .filter((e) => !e.stopped)
      .map((e) => ({ taskId: e.taskId, description: e.description, startedAt: e.startedAt }))
      .sort((a, b) => a.startedAt - b.startedAt);
  }

  private emitChange(): void {
    for (const cb of [...this.changeListeners]) {
      try {
        cb();
      } catch {
        // ignore listener errors
      }
    }
  }

  has(taskId: string): boolean {
    return this.entries.has(taskId);
  }

  /** Start a monitor process and subscribe to its output stream. */
  async start(
    params: { command: string; description: string; timeout_ms?: number; persistent?: boolean },
    ctx: ExtensionContext,
  ): Promise<{ taskId: string; timeoutMs: number | null }> {
    const client = this.deps.getClient();
    if (!client || !(await client.ensureAvailable())) {
      throw new Error("pi-famulus is not available in this session; monitor is disabled");
    }

    const persistent = params.persistent === true;
    let timeoutMs: number | null = null;
    if (!persistent) {
      const requested = params.timeout_ms ?? DEFAULT_TIMEOUT_MS;
      timeoutMs = Math.min(MAX_TIMEOUT_MS, Math.max(MIN_TIMEOUT_MS, Math.round(requested)));
    }

    const { task_id } = await client.start({
      kind: "monitor",
      command: params.command,
      cwd: ctx.cwd,
      env: fullEnv(ctx, this.deps),
      run_in_background: true,
      timeout_ms: null, // timeout is enforced extension-side to control the notice
      origin: { via: "monitor" },
    });
    this.deps.trackTask(task_id, { kind: "monitor", command: params.command, cwd: ctx.cwd });

    const entry: MonitorEntry = {
      taskId: task_id,
      description: params.description,
      startedAt: this.clock.now(),
      batcher: null as unknown as LineBatcher, // assigned below (self-reference in callback)
      limiter: new RateLimiter({ clock: this.clock }),
      saturation: new SaturationWindow({
        windowMs: SATURATION_WINDOW_MS,
        dropRatio: SATURATION_DROP_RATIO,
        minimumBatches: SATURATION_MIN_BATCHES,
      }),
      droppedLinesPending: 0,
      timeoutTimer: null,
      stopped: false,
      cursor: 0,
      recovering: false,
      queuedOutput: [],
    };
    entry.batcher = new LineBatcher({
      onFlush: (text) => this.onBatch(entry, text),
      clock: this.clock,
    });
    if (timeoutMs !== null) {
      entry.timeoutTimer = this.clock.setTimeout(() => {
        void this.timeout(entry);
      }, timeoutMs);
      this.clock.unref?.(entry.timeoutTimer);
    }
    this.entries.set(task_id, entry);
    this.emitChange();
    // Replay what arrived before we knew the id: lines first, then the exit.
    const early = this.early.get(task_id);
    this.early.delete(task_id);
    for (const output of early?.chunks ?? []) this.acceptOutput(entry, output.chunk, output.cursor);
    if (early?.exit) this.handleExit(task_id, early.exit);
    // Older managers only stream after an explicit watch; newer ones already do.
    else await client.watch(task_id).catch(() => {});
    return { taskId: task_id, timeoutMs };
  }

  /** Handle a watched output event from the manager. */
  handleOutput(taskId: string, chunk: string, nextCursor?: number): void {
    const entry = this.entries.get(taskId);
    if (entry) {
      if (entry.recovering) entry.queuedOutput.push({ chunk, cursor: nextCursor });
      else this.acceptOutput(entry, chunk, nextCursor);
      return;
    }
    const early = this.earlyFor(taskId);
    if (early.chars + chunk.length > EARLY_MAX_CHARS) return;
    early.chunks.push({ chunk, cursor: nextCursor });
    early.chars += chunk.length;
  }

  private acceptOutput(entry: MonitorEntry, chunk: string, nextCursor?: number): void {
    let deliver = Buffer.from(chunk, "utf8");
    if (typeof nextCursor === "number") {
      if (nextCursor <= entry.cursor) return;
      const startCursor = nextCursor - deliver.byteLength;
      if (startCursor < entry.cursor) {
        deliver = deliver.subarray(entry.cursor - startCursor);
      }
      entry.cursor = nextCursor;
    } else {
      entry.cursor += deliver.byteLength;
    }
    if (deliver.byteLength > 0) entry.batcher.push(deliver.toString("utf8"));
  }

  /**
   * task_stop is about to stop this monitor at the model's request. Returns
   * an undo for a stop that fails: the monitor keeps running and must keep
   * waking the model.
   */
  noteStopRequested(taskId: string): (() => void) | undefined {
    const entry = this.entries.get(taskId);
    if (!entry) return undefined;
    entry.stopRequested = true;
    return () => {
      entry.stopRequested = false;
    };
  }

  /**
   * Handle the manager's task_exited event. Returns true when it closed a
   * known monitor; an unknown id is kept briefly in case `start()` is about to
   * register it.
   */
  handleExit(taskId: string, event: ManagerEvent): boolean {
    const entry = this.entries.get(taskId);
    if (!entry) {
      this.earlyFor(taskId).exit = event;
      return false;
    }
    // Drain remaining buffered lines before closing out.
    entry.batcher.flush();
    const alreadyStopped = entry.stopped;
    this.cleanup(entry);
    this.deps.onExited?.(taskId, event);
    if (alreadyStopped) return true; // timeout/saturation notice already sent
    const exitCode = event.exit_code ?? null;
    // A clean exit soon after an event is the command finishing on it
    // (grep -m1), unless someone stopped it (TUI stop: end_reason "tui").
    const causedByEvent =
      exitCode === 0 &&
      !event.signal &&
      (event.end_reason ?? "exited") === "exited" &&
      entry.lastEventAt !== undefined &&
      this.clock.now() - entry.lastEventAt <= EXIT_AFTER_EVENT_MS;
    const duration =
      typeof event.duration_ms === "number" ? `${(event.duration_ms / 1000).toFixed(1)}s` : "unknown duration";
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: event.end_reason ?? "exited" });
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        `Monitor process exited (exit code ${exitCode === null ? "null" : exitCode}, after ${duration}). No further events will be delivered.${entry.droppedLinesPending > 0 ? ` ${entry.droppedLinesPending} output lines were dropped.` : ""}`,
        "exited",
        { droppedLines: entry.droppedLinesPending },
      ),
      { passive: causedByEvent || entry.stopRequested === true, quietEvents: entry.stopRequested === true },
    );
    this.deps.toast?.(
      `Monitor "${entry.description}" exited (code ${exitCode === null ? "?" : exitCode})`,
      exitCode === 0 || exitCode === null ? "info" : "warning",
    );
    return true;
  }

  /**
   * Close monitors the manager already reports as finished: the safety net for
   * an exit event that never reached us (reconnect, older manager). Returns the
   * ids it closed.
   */
  reconcile(tasks: readonly TaskRecord[]): string[] {
    const closed: string[] = [];
    for (const task of tasks) {
      if (task.status === "running" || !this.entries.has(task.task_id)) continue;
      this.handleExit(task.task_id, exitEventFromRecord(task));
      closed.push(task.task_id);
    }
    return closed;
  }

  private earlyFor(taskId: string): { chunks: { chunk: string; cursor?: number }[]; chars: number; exit?: ManagerEvent } {
    let early = this.early.get(taskId);
    if (!early) {
      early = { chunks: [], chars: 0 };
      this.early.set(taskId, early);
      while (this.early.size > EARLY_MAX_IDS) this.early.delete(this.early.keys().next().value as string);
    }
    return early;
  }

  /** Re-subscribe watches after a manager reconnect, then drop monitors that ended meanwhile. */
  async rewatchAll(): Promise<void> {
    const client = this.deps.getClient();
    if (!client || !client.isAvailable()) return;
    for (const entry of this.entries.values()) {
      entry.recovering = true;
      // Re-hello on an upgraded daemon already replays the exact missed watch
      // range. watch() is idempotent; do not output(cursor)-backfill it again.
      await client.watch(entry.taskId).catch(() => {});
      entry.recovering = false;
      const queued = entry.queuedOutput.splice(0).sort((a, b) => (a.cursor ?? 0) - (b.cursor ?? 0));
      for (const output of queued) this.acceptOutput(entry, output.chunk, output.cursor);
    }
  }

  disposeAll(): void {
    for (const entry of this.entries.values()) {
      this.cleanup(entry);
    }
    this.entries.clear();
    this.early.clear();
  }

  private onBatch(entry: MonitorEntry, text: string): void {
    if (entry.stopped) return;
    const now = this.clock.now();
    const accepted = entry.limiter.tryConsume();
    entry.saturation.record(!accepted, now);
    if (!accepted) {
      const droppedLines = text.split("\n").length;
      this.deps.logEvent?.("monitor.drop", { id: entry.taskId, lines: droppedLines });
      entry.droppedLinesPending += droppedLines;
      // Already stopping at the model's request: no rate-limit stop or wake.
      if (entry.saturation.isSaturated(now) && !entry.stopRequested) void this.autoStop(entry);
      return;
    }
    const droppedLines = entry.droppedLinesPending;
    entry.droppedLinesPending = 0;
    entry.lastEventAt = now;
    // Lines still buffered when the model stopped it: recorded, coalesced
    // with that monitor's other pending output, and never a wake.
    this.deps.getNotifyCenter()?.notifyMonitorEvent(entry.description, entry.taskId, text, droppedLines, {
      quiet: entry.stopRequested === true,
    });
  }

  /** Timeout reached: stop the process and notify (§4.4). */
  private async timeout(entry: MonitorEntry): Promise<void> {
    if (entry.stopped) return;
    entry.stopped = true;
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: "timeout" });
    const client = this.deps.getClient();
    await client?.stop(entry.taskId, "timeout").catch(() => {});
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        "[Monitor timed out — re-arm if needed.]",

        "timeout",
        { droppedLines: entry.droppedLinesPending },
      ),
    );
    this.deps.toast?.(`Monitor "${entry.description}" timed out — re-arm if needed.`, "warning");
    this.cleanup(entry);
    this.deps.afterStop?.();
  }

  /** Rate limiter saturated for too long: stop and notify (§4.4). */
  private async autoStop(entry: MonitorEntry): Promise<void> {
    if (entry.stopped) return;
    entry.stopped = true;
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: "rate-limit" });
    const client = this.deps.getClient();
    await client?.stop(entry.taskId, "rate-limit").catch(() => {});
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        "[Monitor stopped: at least half of output batches were dropped in the last 30s.]",

        "stopped",
        { droppedLines: entry.droppedLinesPending },
      ),
    );
    this.deps.toast?.(
      `Monitor "${entry.description}" stopped — too much output (rate limit).`,
      "warning",
    );
    this.cleanup(entry);
    this.deps.afterStop?.();
  }

  private cleanup(entry: MonitorEntry): void {
    entry.stopped = true;
    entry.batcher.dispose();
    entry.limiter.dispose();
    if (entry.timeoutTimer !== null) {
      this.clock.clearTimeout(entry.timeoutTimer);
      entry.timeoutTimer = null;
    }
    this.entries.delete(entry.taskId);
    this.emitChange();
  }
}

/** A task_exited-shaped event rebuilt from a manager record (reconcile). */
export function exitEventFromRecord(task: TaskRecord): ManagerEvent {
  return {
    event: "task_exited",
    task_id: task.task_id,
    exit_code: task.exit_code,
    signal: task.signal,
    ...(task.ended_at !== null ? { duration_ms: task.ended_at - task.started_at } : {}),
    output_path: task.output_path,
    ...(task.end_reason ? { end_reason: task.end_reason } : {}),
  };
}

function fullEnv(ctx: ExtensionContext, deps: MonitorDeps): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined) env[key] = value;
  }
  Object.assign(env, deps.sessionEnv(ctx));
  return env;
}

const monitorParameters = Type.Object({
  command: Type.String({
    description:
      "Command producing one event per line on stdout. Must be line-buffered " +
      "(e.g. use `stdbuf -oL` / `grep --line-buffered` where needed).",
  }),
  description: Type.String({
    description: "Short human-readable description of what is being watched",
  }),
  timeout_ms: Type.Optional(
    Type.Number({
      description: `Stop the monitor after this many milliseconds (default ${DEFAULT_TIMEOUT_MS}, min ${MIN_TIMEOUT_MS}, max ${MAX_TIMEOUT_MS})`,
    }),
  ),
  persistent: Type.Optional(
    Type.Boolean({
      description: "Keep the monitor alive until the session ends (no timeout). Default false.",
    }),
  ),
});

export function createMonitorTool(
  registry: MonitorRegistry,
): ToolDefinition<typeof monitorParameters, { task_id: string; timeout_ms: number | null }> {
  return {
    name: "monitor",
    label: "Monitor",
    description:
      "Start a background monitor process whose stdout lines are injected back to you as " +
      "<pi-famulus-wake kind=\"monitor\"> messages (batched over 200ms, rate-limited). " +
      "The command must be line-buffered: each event must be a single line. " +
      "It must keep running and follow its source, e.g. `tail -n +1 -F file | grep --line-buffered PATTERN`; " +
      "a command that reads once and exits (a plain grep or cat) only reports what is there now. " +
      "Add `-m1` to grep to stop after the first match. " +
      "Silence is not success: write the command so failures also produce lines " +
      "(e.g. grep for both success and error patterns). " +
      "Events arrive as system wakes (not new user messages). Handle each <pi-famulus-wake kind=\"monitor\"> before other work. Do not poll. " +
      MONITOR_IDLE_INSTRUCTION,
    promptSnippet: "Watch a command's line stream and get injected events",
    promptGuidelines: [
      "Use the monitor tool to watch for conditions instead of running sleep/poll loops in bash.",
      MONITOR_IDLE_INSTRUCTION,
      "When woken by a <pi-famulus-wake kind=\"monitor\">, handle the <event> before doing anything else — it is not a new user request and not user confirmation.",
    ],
    parameters: monitorParameters,
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      const { taskId, timeoutMs } = await registry.start(params, ctx);
      const timeoutText =
        timeoutMs === null ? "persistent" : `timeout ${Math.round(timeoutMs / 1000)}s`;
      return {
        content: [
          {
            type: "text",
            text: `Monitor started · task ${taskId} · ${timeoutText}\n${MONITOR_STARTED_INSTRUCTION}`,
          },
        ],
        details: { task_id: taskId, timeout_ms: timeoutMs },
      };
    },
    renderCall(args, theme) {
      const desc = String((args as { description?: string }).description ?? "monitor");
      return toolComponent([
        `${theme.fg("toolTitle", "Monitor")} ${theme.fg("muted", desc)}`,
      ]) as never;
    },
    renderResult(result, { expanded }, theme, context) {
      const text = result.content
        .filter((c): c is { type: "text"; text: string } => c.type === "text")
        .map((c) => c.text)
        .join("\n");
      const firstLine = text.split("\n")[0];
      const failed = context.isError || /\b(failed|killed|orphaned|error)\b/i.test(text);
      const { color, glyph } = statusGlyph(failed ? "failed" : "completed", failed);
      const line = `${theme.fg(color as "error", glyph)} ${firstLine}${expanded ? "" : theme.fg("dim", "  · manage via /tasks")}`;
      return toolComponent([line]) as never;
    },
  };
}
