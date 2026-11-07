/**
 * monitor tool (design doc §4.4).
 *
 * Starts a long-lived `kind:"monitor"` process via pi-famulus, watches its
 * output stream, and injects line batches as <pi-famulus-wake kind="monitor"> messages.
 * Batching (LineBatcher) and throttling (RateLimiter) happen extension-side.
 * Sustained drop-ratio or a rolling input line-rate enters summary mode (not stop).
 * Auto-stop is only the unverified ~1000 lines/s for 60s safety backstop.
 */
import { Type } from "typebox";
import { shellStartStatus } from "./shell-start";
import type { ExtensionContext, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { formatDuration, formatExitFacts, formatMonitorEvent } from "./format";
import { MONITOR_IDLE_INSTRUCTION } from "./behavior-guidelines";
import { realClock, type Clock, type ClockTimer } from "./clock";

import type { ManagerClient, ManagerEvent, MonitorOutputRecord, TaskRecord } from "./manager-client";
import { DEFAULT_DEDUP_WINDOW_MS, MonitorOutputBuffer, mergeOutputInfo, type FingerprintMode } from "./monitor-output";
import type { MonitorOutputInfo } from "./wake";
import {
  LineBatcher,
  RateLimiter,
  OverloadController,
  LegacyLineAssembler,
  LEGACY_PARTIAL_MAX_CHARS,
  OVERLOAD_SUMMARY_LINES_PER_SEC,
  OVERLOAD_WINDOW_MS,
  SUMMARY_CADENCE_MS,
  type OverloadControllerOptions,
  type OverloadEnterReason,
  type OverloadInput,
  type OverloadSummaryOutput,
} from "./monitor-batching";
import type { NotifyCenter } from "./notify";
import { fill, fillList, PROMPTS } from "./prompts.generated";
import { statusGlyph, toolComponent } from "./tui/tool-component";
import type { WorkIndex } from "./work-index";


const DEFAULT_TIMEOUT_MS = 300_000;
const MIN_TIMEOUT_MS = 1_000;
const MAX_TIMEOUT_MS = 3_600_000;
/** stall_after_ms bounds: shorter than 10s is a heartbeat; a day is the longest useful silence. */
const MIN_STALL_MS = 10_000;
const MAX_STALL_MS = 86_400_000;
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
export const MONITOR_STARTED_INSTRUCTION = fill("tools.monitor.result.started", { monitorIdle: MONITOR_IDLE_INSTRUCTION });
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
  /** Optional overload-policy overrides for tests. Production uses draft constants. */
  overload?: Partial<OverloadControllerOptions>;
}

interface MonitorEntry {
  taskId: string;
  description: string;
  startedAt: number;
  batcher: LineBatcher;
  output: MonitorOutputBuffer;
  legacyOutput: MonitorOutputInfo;
  recordCursor: number;
  lastRecordKind?: "line" | "progress";
  limiter: RateLimiter;
  overload: OverloadController;
  legacyLines: LegacyLineAssembler;
  droppedLinesPending: number;
  timeoutTimer: ClockTimer | null;
  /** Epoch ms when the extension-side timeout fires; null when persistent. */
  timeoutDeadlineAt: number | null;
  stopped: boolean;
  cursor: number;
  recovering: boolean;
  queuedOutput: { chunk: string; cursor?: number; records?: MonitorOutputRecord[] }[];
  lastEventAt?: number;
  /** Last accepted line or newline-completed raw chunk, including later-deduped/rate-limited output. */
  lastOutputAt?: number;
  /** The model asked to stop it (task_stop): what is left is not news. */
  stopRequested?: boolean;
  /** Prevents autoStop from re-entering while it drains the 200ms batcher. */
  stopping?: boolean;
  /**
   * wake_on: only lines matching it reach the batchers, dedup, overload
   * accounting and wakes. Every line stays in the manager's task log.
   */
  wakeOn: RegExp | null;
  stallAfterMs: number | null;
  stallTimer: ClockTimer | null;
  /** Last new output (any line or progress record, matching wake_on or not). */
  lastActivityAt: number;
  /** The stall notice went out; the next output re-arms it. */
  stallNotified: boolean;
}

export class MonitorRegistry {
  private readonly deps: MonitorDeps;
  private readonly clock: Clock;
  private readonly entries = new Map<string, MonitorEntry>();
  private readonly changeListeners = new Set<() => void>();
  private readonly early = new Map<string, { chunks: { chunk: string; cursor?: number; records?: MonitorOutputRecord[] }[]; chars: number; exit?: ManagerEvent }>();

  constructor(deps: MonitorDeps) {
    this.deps = deps;
    this.clock = deps.clock ?? realClock;
  }

  /** Subscribe to start/stop transitions (fleet status refresh). */
  onChange(cb: () => void): () => void {
    this.changeListeners.add(cb);
    return () => this.changeListeners.delete(cb);
  }

  /** Active monitors for the fleet status surface and task_list extras. */
  listActive(): { taskId: string; description: string; startedAt: number; remainingMs: number | null; persistent: boolean; lastEventAt?: number; lastOutputAt?: number; wakeOn?: string; stallAfterMs?: number }[] {
    const now = this.clock.now();
    return [...this.entries.values()]
      .filter((e) => !e.stopped)
      .map((e) => ({
        taskId: e.taskId,
        description: e.description,
        startedAt: e.startedAt,
        persistent: e.timeoutDeadlineAt === null,
        remainingMs: e.timeoutDeadlineAt === null ? null : Math.max(0, e.timeoutDeadlineAt - now),
        lastEventAt: e.lastEventAt,
        lastOutputAt: e.lastOutputAt,
        ...(e.wakeOn ? { wakeOn: e.wakeOn.source } : {}),
        ...(e.stallAfterMs !== null ? { stallAfterMs: e.stallAfterMs } : {}),
      }))
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
    params: { command: string; description: string; timeout_ms?: number; persistent?: boolean; dedup_window_ms?: number; fingerprint_mode?: FingerprintMode; wake_on?: string; stall_after_ms?: number },
    ctx: ExtensionContext,
  ): Promise<{ taskId: string; timeoutMs: number | null }> {
    const client = this.deps.getClient();
    if (!client || !(await client.ensureAvailable())) {
      throw new Error("pi-famulus is not available in this session; monitor is disabled");
    }

    if (params.dedup_window_ms !== undefined && (!Number.isSafeInteger(params.dedup_window_ms) || params.dedup_window_ms < 0 || params.dedup_window_ms > 3_600_000)) {
      throw new RangeError("dedup_window_ms must be an integer between 0 and 3600000");
    }
    if (params.fingerprint_mode !== undefined && params.fingerprint_mode !== "exact" && params.fingerprint_mode !== "ignore-timestamps") {
      throw new RangeError("fingerprint_mode must be exact or ignore-timestamps");
    }
    if (params.persistent === true && params.timeout_ms !== undefined) {
      throw new RangeError("timeout_ms and persistent are exclusive: use timeout_ms for a watch that ends within 1 hour, persistent:true for an open-ended or longer watch");
    }
    if (params.timeout_ms !== undefined && params.timeout_ms > MAX_TIMEOUT_MS) {
      throw new RangeError("timeout_ms exceeds the 1-hour maximum; for longer watches use persistent:true instead of re-arming");
    }
    let wakeOn: RegExp | null = null;
    if (params.wake_on !== undefined) {
      if (typeof params.wake_on !== "string" || params.wake_on.length === 0) {
        throw new RangeError("wake_on must be a non-empty regular expression source");
      }
      try {
        wakeOn = new RegExp(params.wake_on);
      } catch (error) {
        throw new RangeError(`wake_on is not a valid regular expression: ${error instanceof Error ? error.message : String(error)}`);
      }
    }
    if (params.stall_after_ms !== undefined && (!Number.isSafeInteger(params.stall_after_ms) || params.stall_after_ms < MIN_STALL_MS || params.stall_after_ms > MAX_STALL_MS)) {
      throw new RangeError(`stall_after_ms must be an integer between ${MIN_STALL_MS} and ${MAX_STALL_MS}`);
    }
    // No lifetime given: run until the command exits or the monitor is stopped.
    // persistent:false without timeout_ms keeps the 5-minute default.
    const persistent = params.persistent === true || (params.persistent !== false && params.timeout_ms === undefined);
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
      label: params.description,
    });
    this.deps.trackTask(task_id, { kind: "monitor", command: params.command, cwd: ctx.cwd });
    this.deps.logEvent?.("monitor.start", { id: task_id, description: params.description });

    const now = this.clock.now();
    const entry: MonitorEntry = {
      taskId: task_id,
      description: params.description,
      startedAt: now,
      batcher: null as unknown as LineBatcher, // assigned below (self-reference in callback)
      output: null as unknown as MonitorOutputBuffer,
      legacyOutput: {},
      recordCursor: -1,
      limiter: new RateLimiter({ clock: this.clock }),
      overload: null as unknown as OverloadController,
      legacyLines: new LegacyLineAssembler(),
      droppedLinesPending: 0,
      timeoutTimer: null,
      timeoutDeadlineAt: timeoutMs === null ? null : now + timeoutMs,
      stopped: false,
      cursor: 0,
      recovering: false,
      queuedOutput: [],
      wakeOn,
      stallAfterMs: params.stall_after_ms ?? null,
      stallTimer: null,
      lastActivityAt: now,
      stallNotified: false,
    };
    entry.overload = new OverloadController({
      clock: this.clock,
      ...this.deps.overload,
      onSummary: (text, output) => this.emitOverloadNotice(entry, text, output),
      onEnter: (reason) => this.emitOverloadNotice(entry, this.enterNotice(reason)),
      onExit: () => this.emitOverloadNotice(entry, "[Monitor overload: leaving summary mode. Individual lines resume.]"),
    });
    entry.batcher = new LineBatcher({
      onFlush: (text, info) => {
        const output = mergeOutputInfo(entry.legacyOutput, info);
        entry.legacyOutput = {};
        this.onBatch(entry, text, output);
      },
      clock: this.clock,
    });
    entry.output = new MonitorOutputBuffer({
      clock: this.clock,
      windowMs: params.dedup_window_ms ?? DEFAULT_DEDUP_WINDOW_MS,
      mode: params.fingerprint_mode ?? "exact",
      onFlush: ({ text, output }) => this.onBatch(entry, text, output, entry.output.onDelivered),
      onSuppressed: () => this.deps.logEvent?.("monitor.dedupe", { id: entry.taskId }),
    });
    if (timeoutMs !== null) {
      entry.timeoutTimer = this.clock.setTimeout(() => {
        void this.timeout(entry);
      }, timeoutMs);
      this.clock.unref?.(entry.timeoutTimer);
    }
    if (entry.stallAfterMs !== null) this.armStall(entry, entry.stallAfterMs);
    this.entries.set(task_id, entry);
    this.emitChange();
    // Replay what arrived before we knew the id: lines first, then the exit.
    const early = this.early.get(task_id);
    this.early.delete(task_id);
    for (const output of early?.chunks ?? []) this.acceptOutput(entry, output.chunk, output.cursor, output.records);
    if (early?.exit) this.handleExit(task_id, early.exit);
    // Older managers only stream after an explicit watch; newer ones already do.
    else await client.watch(task_id).catch(() => {});
    return { taskId: task_id, timeoutMs };
  }

  /** Handle a watched output event from the manager. */
  handleOutput(taskId: string, chunk: string, nextCursor?: number, records?: MonitorOutputRecord[]): void {
    const entry = this.entries.get(taskId);
    if (entry) {
      if (entry.recovering) entry.queuedOutput.push({ chunk, cursor: nextCursor, records });
      else this.acceptOutput(entry, chunk, nextCursor, records);
      return;
    }
    const early = this.earlyFor(taskId);
    const cost = chunk.length + (records?.reduce((n, r) => n + r.preview.length + 256, 0) ?? 0);
    if (early.chars + cost > EARLY_MAX_CHARS) {
      // Metadata expansion (e.g. hundreds of empty lines) must not discard
      // startup bytes which fit the budget. Convert the buffered prefix too:
      // it may contain raw fragments held for a later completed-line record.
      const rawCost = early.chunks.reduce((n, output) => n + output.chunk.length, 0) + chunk.length;
      if (rawCost > EARLY_MAX_CHARS) return;
      for (const output of early.chunks) output.records = undefined;
      early.chunks.push({ chunk, cursor: nextCursor });
      early.chars = rawCost;
    } else {
      early.chunks.push({ chunk, cursor: nextCursor, records });
      early.chars += cost;
    }
  }

  private acceptOutput(entry: MonitorEntry, chunk: string, nextCursor?: number, records?: MonitorOutputRecord[]): void {
    let deliver = Buffer.from(chunk, "utf8");
    const previousCursor = entry.cursor;
    if (typeof nextCursor === "number") {
      if (nextCursor <= entry.cursor) deliver = Buffer.alloc(0);
      else {
        const startCursor = nextCursor - deliver.byteLength;
        if (startCursor < entry.cursor) deliver = deliver.subarray(entry.cursor - startCursor);
        entry.cursor = nextCursor;
      }
    } else entry.cursor += deliver.byteLength;
    if (records !== undefined) {
      // Empty metadata arrays deliberately hold raw fragments for Rust framing.
      entry.batcher.flush();
      const accepted: MonitorOutputRecord[] = [];
      let filteredLines = 0;
      let metadataTookOver = false;
      for (const record of records) {
        if (!validRecord(record, entry.cursor)) continue;
        if (record.cursor_end < entry.recordCursor || (record.cursor_end === entry.recordCursor && !(entry.lastRecordKind === "progress" && record.kind === "line"))) continue;
        metadataTookOver = true;
        entry.recordCursor = record.cursor_end;
        entry.lastRecordKind = record.kind;
        if (record.kind === "line") entry.lastOutputAt = this.clock.now();
        this.noteActivity(entry);
        // Filtered-out records reach neither dedup nor wake accounting,
        // only the stop backstop.
        if (!admits(entry, record.preview)) {
          if (record.kind === "line") filteredLines++;
          continue;
        }
        entry.output.push(record);
        if (record.kind === "line") accepted.push(record);
      }
      if (metadataTookOver) entry.legacyLines.reset();
      this.noteFilteredLines(entry, filteredLines);
      if (accepted.length > 0) {
        this.noteInput(entry, {
          lines: accepted.length,
          bytes: accepted.reduce((n, r) => n + (r.cursor_end - r.cursor_start), 0),
          text: accepted.map((r) => r.preview).join("\n"),
          cursorStart: Math.min(...accepted.map((r) => r.cursor_start)),
          cursorEnd: Math.max(...accepted.map((r) => r.cursor_end)),
        });
      }
    } else if (deliver.byteLength > 0) {
      // Legacy manager or unavailable replay metadata: fail open, never hash in JS.
      entry.output.flush();
      const cursorStart = Math.max(previousCursor, entry.cursor - deliver.byteLength);
      const text = deliver.toString("utf8");
      if (text.includes("\n")) {
        entry.lastOutputAt = this.clock.now();
        this.noteActivity(entry);
      }
      const assembled = entry.legacyLines.push(text, deliver.byteLength, cursorStart, entry.cursor);
      if (entry.wakeOn) {
        // Raw chunks are filtered per completed line; a trailing fragment
        // waits for its newline (or the exit).
        if (assembled) this.admitLegacyLines(entry, assembled);
        return;
      }
      entry.legacyOutput = mergeOutputInfo(entry.legacyOutput, {
        firstObservedAt: this.clock.now(), lastObservedAt: this.clock.now(),
        cursorStart, cursorEnd: entry.cursor,
      });
      if (assembled) {
        this.noteInput(entry, {
          lines: assembled.lines,
          bytes: assembled.bytes,
          text: assembled.text ?? "",
          cursorStart: assembled.cursorStart ?? cursorStart,
          cursorEnd: assembled.cursorEnd ?? entry.cursor,
        });
      }
      entry.batcher.push(text);
    }
  }

  /** wake_on on the raw path: pass only matching completed lines on to overload accounting and the batcher. */
  private admitLegacyLines(entry: MonitorEntry, assembled: OverloadInput): void {
    const all = (assembled.text ?? "").split("\n");
    const lines = all.filter((line) => admits(entry, line));
    this.noteFilteredLines(entry, all.length - lines.length);
    if (lines.length === 0) return;
    const now = this.clock.now();
    entry.legacyOutput = mergeOutputInfo(entry.legacyOutput, {
      firstObservedAt: now, lastObservedAt: now,
      cursorStart: assembled.cursorStart, cursorEnd: assembled.cursorEnd,
      // The assembler keeps at most LEGACY_PARTIAL_MAX_CHARS of a line.
      truncated: lines.some((line) => line.length >= LEGACY_PARTIAL_MAX_CHARS),
    });
    this.noteInput(entry, {
      lines: lines.length,
      bytes: lines.reduce((n, line) => n + Buffer.byteLength(line) + 1, 0),
      text: lines.join("\n"),
      cursorStart: assembled.cursorStart ?? entry.cursor,
      cursorEnd: assembled.cursorEnd ?? entry.cursor,
    });
    entry.batcher.push(lines.map((line) => `${line}\n`).join(""));
  }

  /** Any new output proves the source is alive: re-arm a stall notice that already went out. */
  private noteActivity(entry: MonitorEntry): void {
    entry.lastActivityAt = this.clock.now();
    if (entry.stallAfterMs !== null && entry.stallNotified) {
      entry.stallNotified = false;
      this.armStall(entry, entry.stallAfterMs);
    }
  }

  private armStall(entry: MonitorEntry, delayMs: number): void {
    if (entry.stallTimer !== null) this.clock.clearTimeout(entry.stallTimer);
    entry.stallTimer = this.clock.setTimeout(() => this.onStallDue(entry), delayMs);
    this.clock.unref?.(entry.stallTimer);
  }

  /** One timer per monitor, re-checked lazily, so output never touches timers. */
  private onStallDue(entry: MonitorEntry): void {
    entry.stallTimer = null;
    if (entry.stopped || entry.stallAfterMs === null) return;
    const idle = this.clock.now() - entry.lastActivityAt;
    if (idle < entry.stallAfterMs) {
      this.armStall(entry, entry.stallAfterMs - idle);
      return;
    }
    entry.stallNotified = true;
    if (entry.stopRequested || entry.stopping) return;
    const center = this.deps.getNotifyCenter();
    if (!center) return;
    this.deps.logEvent?.("monitor.stall", { id: entry.taskId, idleMs: idle });
    center.notifyMonitorEvent(entry.description, entry.taskId, fill("tools.monitor.event.stalled", { idle: formatDuration(idle) }), 0, {});
  }

  /** Lines wake_on kept from waking still count toward the input-rate stop backstop. */
  private noteFilteredLines(entry: MonitorEntry, lines: number): void {
    if (lines <= 0) return;
    entry.overload.recordFilteredLines(lines);
    if (entry.overload.shouldStop() && !entry.stopRequested && !entry.stopping) void this.autoStop(entry);
  }

  /** Count complete raw lines/chunks before limiter (includes later-deduped lines). */
  private noteInput(entry: MonitorEntry, input: { lines: number; bytes: number; text: string; cursorStart: number; cursorEnd: number }): void {
    entry.overload.recordInput(input);
    if (entry.overload.shouldStop() && !entry.stopRequested && !entry.stopping) void this.autoStop(entry);
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
    // Drain remaining buffered lines before closing out. Unfiltered raw
    // output already holds the trailing fragment in the batcher.
    if (entry.wakeOn) {
      const partial = entry.legacyLines.drainPartial();
      if (partial) this.admitLegacyLines(entry, partial);
    }
    entry.batcher.flush();
    entry.output.flush();
    const alreadyStopped = entry.stopped;
    if (!alreadyStopped && !entry.stopRequested) entry.overload.flushSummary();
    this.cleanup(entry);
    this.deps.onExited?.(taskId, event);
    if (alreadyStopped) return true; // timeout/saturation owns the pending summary
    const summary = entry.output.takeSummary();
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
    const facts = {
      ...(event.end_reason ? { endReason: event.end_reason } : {}),
      ...(event.signal ? { signal: event.signal } : {}),
      exitCode,
      ...(entry.stopRequested ? { stopRequested: true } : {}),
    };
    const exitContext = `After ${duration}.${startFailureNote(event.end_reason)} No further events will be delivered.${entry.droppedLinesPending > 0 ? ` ${entry.droppedLinesPending} output lines were dropped.` : ""}`;
    const detail = `${formatExitFacts(facts)} ${exitContext}`;
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: event.end_reason ?? "unknown" });
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        detail,
        "exited",
        { droppedLines: entry.droppedLinesPending, exitFacts: facts, exitContext, ...(summary.suppressedCount ? { output: summary } : {}) },
      ),
      { passive: causedByEvent || entry.stopRequested === true, quietEvents: entry.stopRequested === true },
    );
    this.deps.toast?.(
      `Monitor "${entry.description}": ${detail}`,
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

  private earlyFor(taskId: string): { chunks: { chunk: string; cursor?: number; records?: MonitorOutputRecord[] }[]; chars: number; exit?: ManagerEvent } {
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
      for (const output of queued) this.acceptOutput(entry, output.chunk, output.cursor, output.records);
    }
  }

  disposeAll(): void {
    for (const entry of this.entries.values()) {
      this.cleanup(entry);
    }
    this.entries.clear();
    this.early.clear();
  }

  private enterNotice(reason: OverloadEnterReason): string {
    const windowMs = this.deps.overload?.windowMs ?? OVERLOAD_WINDOW_MS;
    const cadenceMs = this.deps.overload?.cadenceMs?.[0] ?? SUMMARY_CADENCE_MS[0];
    const linesPerSec = this.deps.overload?.summaryLinesPerSec ?? OVERLOAD_SUMMARY_LINES_PER_SEC;
    const windowSecs = Math.round(windowMs / 1000);
    const why = reason === "drop-ratio"
      ? `drop-ratio ≥50% over ${windowSecs}s`
      : reason === "line-rate"
        ? `input >${linesPerSec} lines/s over ${windowSecs}s`
        : `drop-ratio ≥50% over ${windowSecs}s and input >${linesPerSec} lines/s`;
    return `[Monitor overload: entering summary mode (${why}). Individual lines are suppressed. First summary after ${Math.round(cadenceMs / 1000)}s. Raw output via task_output.]`;
  }

  private emitOverloadNotice(entry: MonitorEntry, text: string, output?: OverloadSummaryOutput): void {
    if (entry.stopped || entry.stopRequested) return;
    const center = this.deps.getNotifyCenter();
    if (!center) return;
    center.notifyMonitorEvent(entry.description, entry.taskId, text, 0, output ? { output } : {});
    entry.lastEventAt = this.clock.now();
  }

  private onBatch(entry: MonitorEntry, text: string, output?: MonitorOutputInfo, onDelivered?: () => void): boolean {
    if (entry.stopped) return false;
    const center = this.deps.getNotifyCenter();
    if (!center) return false;
    const now = this.clock.now();
    if (entry.overload.shouldStop() && !entry.stopRequested && !entry.stopping) {
      void this.autoStop(entry);
      return false;
    }
    const accepted = entry.limiter.tryConsume();
    entry.overload.recordBatch(!accepted);
    if (entry.overload.shouldStop() && !entry.stopRequested && !entry.stopping) {
      void this.autoStop(entry);
      return false;
    }
    if (!accepted) {
      const droppedLines = text.split("\n").length;
      this.deps.logEvent?.("monitor.drop", { id: entry.taskId, lines: droppedLines });
      entry.droppedLinesPending += droppedLines;
    }
    if (entry.overload.inSummary()) return false;
    if (!accepted) return false;
    const droppedLines = entry.droppedLinesPending;
    entry.droppedLinesPending = 0;
    entry.lastEventAt = now;
    // Lines still buffered when the model stopped it: recorded, coalesced
    // with that monitor's other pending output, and never a wake.
    center.notifyMonitorEvent(entry.description, entry.taskId, text, droppedLines, {
      quiet: entry.stopRequested === true,
      output,
      onDelivered,
    });
    return true;
  }

  /** Timeout reached: stop the process and notify (§4.4). */
  private async timeout(entry: MonitorEntry): Promise<void> {
    if (entry.stopped) return;
    entry.batcher.flush();
    entry.output.flush();
    if (entry.stopped) return; // A flush may already have tripped overload protection.
    entry.overload.flushSummary();
    entry.stopped = true;
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: "timeout" });
    const client = this.deps.getClient();
    let stopFailed = false;
    let stopError: unknown;
    if (client) await client.stop(entry.taskId, "timeout").catch((error: unknown) => { stopFailed = true; stopError = error; });
    const stopDetail = stopFailed ? `Stop request failed: ${String(stopError)}. Exit unconfirmed.` : client ? "Stop requested; exit unconfirmed." : "Stop request unavailable; exit unconfirmed.";
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        `Timeout elapsed. ${stopDetail}`,
        "timeout",
        { droppedLines: entry.droppedLinesPending, output: entry.output.takeSummary() },
      ),
    );
    this.deps.toast?.(`Monitor "${entry.description}": timeout elapsed. ${stopDetail}`, "warning");
    this.cleanup(entry);
    this.deps.afterStop?.();
  }

  /** Extreme sustained input rate: stop and notify (unverified safety backstop). */
  private async autoStop(entry: MonitorEntry): Promise<void> {
    if (entry.stopped || entry.stopping) return;
    entry.stopping = true;
    entry.batcher.flush();
    entry.output.flush();
    if (entry.stopped) return;
    entry.overload.flushSummary();
    entry.stopped = true;
    this.deps.logEvent?.("monitor.stop", { id: entry.taskId, reason: "rate-limit" });
    const client = this.deps.getClient();
    let stopFailed = false;
    let stopError: unknown;
    if (client) await client.stop(entry.taskId, "rate-limit").catch((error: unknown) => { stopFailed = true; stopError = error; });
    const stopDetail = stopFailed ? `Stop request failed: ${String(stopError)}. Exit unconfirmed.` : client ? "Stop requested; exit unconfirmed." : "Stop request unavailable; exit unconfirmed.";
    this.deps.getNotifyCenter()?.notify(
      formatMonitorEvent(
        entry.description,
        entry.taskId,
        `${entry.overload.stopNotice()} ${stopDetail}`,
        "stopped",
        { droppedLines: entry.droppedLinesPending, output: entry.output.takeSummary() },
      ),
    );
    this.deps.toast?.(
      `Monitor "${entry.description}": input rate safety backstop. ${stopDetail}`,
      "warning",
    );
    this.cleanup(entry);
    this.deps.afterStop?.();
  }

  private cleanup(entry: MonitorEntry): void {
    entry.stopped = true;
    entry.batcher.dispose();
    entry.output.dispose();
    entry.limiter.dispose();
    entry.overload.dispose();
    if (entry.timeoutTimer !== null) {
      this.clock.clearTimeout(entry.timeoutTimer);
      entry.timeoutTimer = null;
    }
    if (entry.stallTimer !== null) {
      this.clock.clearTimeout(entry.stallTimer);
      entry.stallTimer = null;
    }
    this.entries.delete(entry.taskId);
    this.emitChange();
  }
}

function admits(entry: MonitorEntry, line: string): boolean {
  return entry.wakeOn === null || entry.wakeOn.test(line);
}

function validRecord(record: MonitorOutputRecord, cursor: number): boolean {
  return record?.version === 1 && (record.kind === "line" || record.kind === "progress") &&
    typeof record.preview === "string" && record.preview.length <= 2000 &&
    Number.isSafeInteger(record.cursor_start) && record.cursor_start >= 0 &&
    Number.isSafeInteger(record.cursor_end) && record.cursor_end >= record.cursor_start && record.cursor_end <= cursor &&
    Number.isFinite(record.observed_at) && record.observed_at >= 0;
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
  command: Type.String({ description: PROMPTS["tools.monitor.param.command"] }),
  description: Type.String({ description: PROMPTS["tools.monitor.param.description"] }),
  timeout_ms: Type.Optional(
    Type.Number({
      description: fill("tools.monitor.param.timeout_ms", {
        defaultMs: DEFAULT_TIMEOUT_MS,
        minMs: MIN_TIMEOUT_MS,
        maxMs: MAX_TIMEOUT_MS,
      }),
    }),
  ),
  persistent: Type.Optional(Type.Boolean({ description: PROMPTS["tools.monitor.param.persistent"] })),
  dedup_window_ms: Type.Optional(Type.Integer({ minimum: 0, maximum: 3_600_000, description: PROMPTS["tools.monitor.param.dedup_window_ms"] })),
  fingerprint_mode: Type.Optional(Type.Union([Type.Literal("exact"), Type.Literal("ignore-timestamps")], { description: PROMPTS["tools.monitor.param.fingerprint_mode"] })),
  wake_on: Type.Optional(Type.String({ minLength: 1, description: PROMPTS["tools.monitor.param.wake_on"] })),
  stall_after_ms: Type.Optional(
    Type.Integer({
      minimum: MIN_STALL_MS,
      maximum: MAX_STALL_MS,
      description: fill("tools.monitor.param.stall_after_ms", { minMs: MIN_STALL_MS, maxMs: MAX_STALL_MS }),
    }),
  ),
});

export function createMonitorTool(
  registry: MonitorRegistry,
  getIndex?: () => WorkIndex | null,
): ToolDefinition<typeof monitorParameters, { task_id: string; timeout_ms: number | null }> {
  return {
    name: "monitor",
    label: "Monitor",
    description: fill("tools.monitor.description", { monitorIdle: MONITOR_IDLE_INSTRUCTION }),
    promptSnippet: PROMPTS["tools.monitor.snippet"],
    promptGuidelines: fillList("tools.monitor.rules", { monitorIdle: MONITOR_IDLE_INSTRUCTION }),
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
      // Live status from the work index (a started monitor is running, not
      // done); the tool's own error flag when the start itself failed.
      const taskId = (result.details as { task_id?: string } | undefined)?.task_id;
      const index = getIndex?.() ?? null;
      const item = taskId ? index?.get(taskId) : undefined;
      const state = context.state as { unsubscribe?: () => void } | undefined;
      if (index && taskId && state && !state.unsubscribe && (item?.status === "running" || item?.status === "pending")) {
        state.unsubscribe = index.onChange(() => {
          const now = index.get(taskId)?.status;
          if (now !== "running" && now !== "pending") {
            state.unsubscribe?.();
            state.unsubscribe = () => {};
          }
          context.invalidate?.();
        });
      }
      // Live monitors are indexed before this renders, so a missing row is a
      // finished one that was pruned or predates a reload.
      const status = context.isError ? "failed" : item?.status ?? (taskId && !index ? "running" : "completed");
      const { color, glyph } = statusGlyph(status, context.isError);
      const line = `${theme.fg(color as "error", glyph)} ${firstLine}${expanded ? "" : theme.fg("dim", " · /tasks")}`;
      return toolComponent([line]) as never;
    },
  };
}

/** " <why>" when the monitor's Git Bash never started; "" otherwise. */
function startFailureNote(endReason: string | undefined): string {
  const status = shellStartStatus(endReason);
  return status ? ` ${status}` : "";
}
