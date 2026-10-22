/**
 * NotifyCenter (design doc §4.5): the single injection point for all
 * asynchronous events into the pi session.
 *
 * - idle  -> pi.sendMessage(msg, { triggerTurn: true })
 * - busy  -> pi.sendMessage(msg, { deliverAs: "steer" })
 * - passive -> held until the agent settles, then
 *   pi.sendMessage(msg, { triggerTurn: false }): appended, starting no turn.
 *   Sent at once while the model was writing, pi appended it at that turn's
 *   end, while a wake steered in at the same moment waits for the next turn's
 *   start: a monitor's exit landed before its own event (eval batches 2–4).
 * - task exit notifications are coalesced over a 200ms window into one
 *   <pi-famulus-wake kind="task"> payload, and the same task/event pair is only
 *   ever delivered once.
 */
import { formatMonitorEvent, formatTaskNotification, type TaskExitInfo } from "./format";
import { FAMULUS_WAKE_CUSTOM_TYPE, stampWakeAsOf, wakeIds, type MonitorOutputInfo, type WakeItem } from "./wake";
import { realClock, type Clock, type ClockTimer } from "./clock";


type MonitorDeliveryAck = (visiblePreview?: string) => void;

export interface NotifyMessage {
  customType: string;
  content: string;
  details?: unknown;
}

export interface NotifyCenterDeps {
  sendMessage: (
    message: { customType: string; content: string; display: boolean; details?: unknown },
    options: { triggerTurn?: boolean; deliverAs?: "steer" },
  ) => void;
  isIdle: () => boolean;
  /** Batching window for task exit notifications (ms). Default 200. */
  batchMs?: number;
  /**
   * Background tasks still awaiting their own exit wake.
   * Read at flush time so siblings that exit in the same window are not listed.
   */
  listStillRunning?: () => WakeItem[];
  clock?: Clock;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
  /**
   * When a wake is steered because the parent is busy, wait this long and
   * if it is still busy, send the wake as a new turn. Default 45s.
   */
  hangMs?: number;
}


// Reserve 100 of the 3000 preview characters for the event-count prefix and
// omission marker. No raw pending text grows with the number/size of events.
const MONITOR_PREVIEW_CHARS = 2900;
const MONITOR_PREVIEW_HALF = MONITOR_PREVIEW_CHARS / 2;
interface MonitorPreview {
  head: string;
  tail: string;
  omittedChars: number;
}

function clipUtf16Start(text: string, max: number): string {
  if (text.length <= max) return text;
  const out = text.slice(0, max);
  const last = out.charCodeAt(out.length - 1);
  return last >= 0xd800 && last <= 0xdbff ? out.slice(0, -1) : out;
}

function clipUtf16End(text: string, max: number): string {
  if (text.length <= max) return text;
  const out = text.slice(-max);
  const first = out.charCodeAt(0);
  return first >= 0xdc00 && first <= 0xdfff ? out.slice(1) : out;
}

function appendMonitorPreview(previous: MonitorPreview | undefined, event: string): MonitorPreview {
  const total = previous
    ? previous.head.length + previous.tail.length + previous.omittedChars + 1 + event.length
    : event.length;
  if (total <= MONITOR_PREVIEW_CHARS) {
    return { head: previous ? `${previous.head}\n${event}` : event, tail: "", omittedChars: 0 };
  }
  const head = previous
    ? clipUtf16Start(`${previous.head}\n${clipUtf16Start(event, MONITOR_PREVIEW_HALF)}`, MONITOR_PREVIEW_HALF)
    : clipUtf16Start(event, MONITOR_PREVIEW_HALF);
  const tail = event.length >= MONITOR_PREVIEW_HALF
    ? clipUtf16End(event, MONITOR_PREVIEW_HALF)
    : clipUtf16End(`${previous?.tail || previous?.head || ""}\n${event}`, MONITOR_PREVIEW_HALF);
  return { head, tail, omittedChars: total - head.length - tail.length };
}

function mergeMonitorOutput(previous: MonitorOutputInfo | undefined, next: MonitorOutputInfo | undefined): MonitorOutputInfo | undefined {
  if (!previous && !next) return undefined;
  const output = { ...previous };
  for (const key of ["firstObservedAt", "cursorStart"] as const) {
    const value = next?.[key];
    if (value !== undefined) output[key] = Math.min(previous?.[key] ?? value, value);
  }
  for (const key of ["lastObservedAt", "cursorEnd"] as const) {
    const value = next?.[key];
    if (value !== undefined) output[key] = Math.max(previous?.[key] ?? value, value);
  }
  for (const key of ["suppressedCount", "omittedChars"] as const) {
    if (previous?.[key] !== undefined || next?.[key] !== undefined) output[key] = (previous?.[key] ?? 0) + (next?.[key] ?? 0);
  }
  if (previous?.truncated !== undefined || next?.truncated !== undefined) output.truncated = !!previous?.truncated || !!next?.truncated;
  return output;
}

export class NotifyCenter {
  private readonly deps: NotifyCenterDeps;
  private readonly batchMs: number;
  private readonly clock: Clock;
  private pendingExits: TaskExitInfo[] = [];
  private readonly pendingMonitors = new Map<
    string,
    { description: string; eventCount: number; preview: MonitorPreview; droppedLines: number; quiet: boolean; output?: MonitorOutputInfo; onDelivered?: MonitorDeliveryAck }
  >();
  private readonly seen = new Set<string>();
  private timer: ClockTimer | null = null;
  private disposed = false;
  /** A wake started or steered a run that has not settled yet. */
  private runPending = false;
  private held: { message: NotifyMessage; onDelivered?: MonitorDeliveryAck; sourcePreview?: string }[] = [];
  private readonly hangMs: number;
  private hangTimer: ClockTimer | null = null;
  private steered: NotifyMessage | null = null;

  constructor(deps: NotifyCenterDeps) {
    this.deps = deps;
    this.batchMs = deps.batchMs ?? 200;
    this.clock = deps.clock ?? realClock;
    this.hangMs = deps.hangMs ?? 45_000;
  }

  /**
   * Queue a task exit notification. Deduplicated by task id; merged with
   * other exits inside the batching window.
   */
  notifyTaskExit(info: TaskExitInfo): void {
    if (this.disposed) return;
    const key = `exit:${info.taskId}`;
    if (this.seen.has(key)) {
      this.deps.logEvent?.("wake.dedupe", { id: info.taskId });
      return;
    }
    this.seen.add(key);
    this.pendingExits.push(info);
    this.scheduleFlush();
  }

  /**
   * Send a notification immediately, routed by idle/busy state. A monitor's
   * own notice (exit, timeout, stop) first delivers that monitor's coalesced
   * events: they happened before it, and the model must not hear "exited"
   * ahead of the lines the command printed.
   */
  notify(message: NotifyMessage, opts: { passive?: boolean; quietEvents?: boolean } = {}): void {
    if (this.disposed) return;
    // Stamped on arrival: a held passive notice keeps the time it was built.
    message = stampWakeAsOf(message, this.clock.now());
    const wake = message.details as { kind?: string; id?: string } | undefined;
    if (wake?.kind === "monitor" && typeof wake.id === "string") this.flushMonitor(wake.id, opts.quietEvents);
    if (!opts.passive) {
      this.deliver(message);
    } else {
      this.deliverPassive(message);
    }
  }

  private deliverPassive(message: NotifyMessage, onDelivered?: MonitorDeliveryAck, sourcePreview?: string): void {
    message = stampWakeAsOf(message, this.clock.now());
    if (this.runPending || !this.deps.isIdle()) this.held.push({ message, onDelivered, sourcePreview });
    else this.deliver(message, true, onDelivered, sourcePreview);
  }

  /**
   * The agent settled: the run any earlier wake started is over. Flush
   * coalesced monitor output, then, unless that started another run, the
   * held passive notices.
   */
  settled(): void {
    if (this.disposed) return;
    this.runPending = false;
    this.clearHang();
    this.flushMonitorEvents();
    if (this.runPending) return;
    for (const { message, onDelivered, sourcePreview } of this.held.splice(0)) this.deliver(message, true, onDelivered, sourcePreview);
  }

  /**
   * Deliver monitor output immediately when idle, otherwise coalesce per
   * monitor. `quiet`: output of a monitor the model stopped; it is only
   * coalesced, and its summary is delivered without a turn (with the exit
   * notice, or at settle), pending events of that monitor included.
   * `onDelivered` receives surviving source lines after sendMessage returns, not
   * enqueue; generated headings/omission markers are never acknowledgement
   * evidence. Callers must reuse one stable callback per monitor: retaining one
   * callback bounds pending acknowledgements without silently dropping any.
   */
  notifyMonitorEvent(description: string, taskId: string, event: string, droppedLines = 0, opts: { quiet?: boolean; onDelivered?: MonitorDeliveryAck; output?: MonitorOutputInfo } = {}): void {
    if (this.disposed) return;
    const pending = this.pendingMonitors.get(taskId);
    if (pending?.onDelivered && opts.onDelivered && pending.onDelivered !== opts.onDelivered) {
      throw new Error(`Monitor ${taskId} requires a stable onDelivered callback while pending`);
    }
    if (this.deps.isIdle() && !pending && !opts.quiet) {
      const wake = formatMonitorEvent(description, taskId, event, undefined, { droppedLines, output: opts.output });
      this.deliver({ customType: wake.customType, content: wake.content, details: wake.details }, false, opts.onDelivered);
      return;
    }
    this.pendingMonitors.set(taskId, {
      description,
      eventCount: (pending?.eventCount ?? 0) + 1,
      preview: appendMonitorPreview(pending?.preview, event),
      droppedLines: (pending?.droppedLines ?? 0) + droppedLines,
      quiet: (pending?.quiet ?? false) || opts.quiet === true,
      output: mergeMonitorOutput(pending?.output, opts.output),
      onDelivered: pending?.onDelivered ?? opts.onDelivered,
    });
    if (this.deps.isIdle() && !opts.quiet) this.flushMonitorEvents();
  }

  /** Flush coalesced monitor output when the parent agent settles. */
  flushMonitorEvents(): void {
    if (this.disposed || !this.deps.isIdle() || this.pendingMonitors.size === 0) return;
    for (const taskId of [...this.pendingMonitors.keys()]) this.flushMonitor(taskId);
  }

  /** Deliver one monitor's coalesced events, if any, in the current mode. */
  private flushMonitor(taskId: string, quiet = false): void {
    const item = this.pendingMonitors.get(taskId);
    if (!item) return;
    this.pendingMonitors.delete(taskId);
    const { head, tail, omittedChars } = item.preview;
    const preview = omittedChars > 0 ? `${head}\n[... ${omittedChars} characters omitted ...]\n${tail}` : head;
    const summary = item.eventCount > 1 ? `${item.eventCount} events\n${preview}` : preview;
    const wake = formatMonitorEvent(item.description, taskId, summary, undefined, {
      eventCount: item.eventCount,
      droppedLines: item.droppedLines,
      output: mergeMonitorOutput(item.output, omittedChars > 0 ? { truncated: true, omittedChars } : undefined),
    });
    const message = { customType: wake.customType, content: wake.content, details: wake.details };
    // Exclude generated text and boundary fragments: a clipped middle source
    // line named e.g. "30 events" was not seen just because our heading says it.
    const sourcePreview = omittedChars > 0
      ? [
        head.slice(0, Math.max(0, head.lastIndexOf("\n"))),
        tail.includes("\n") ? tail.slice(tail.indexOf("\n") + 1) : "",
      ].filter(Boolean).join("\n")
      : head;
    if (quiet || item.quiet) this.deliverPassive(message, item.onDelivered, sourcePreview);
    else this.deliver(message, false, item.onDelivered, sourcePreview);
  }

  /** Flush any pending task exit notifications now. */
  flush(): void {
    this.clearTimer();
    this.flushExits();
    this.flushMonitorEvents();
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimer();
    this.clearHang();
    this.pendingExits = [];
    this.pendingMonitors.clear();
    this.held = [];
  }

  private scheduleFlush(): void {
    if (this.timer !== null) return;
    this.timer = this.clock.setTimeout(() => {
      this.timer = null;
      this.flushExits();
    }, this.batchMs);
    this.clock.unref?.(this.timer);
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      this.clock.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private flushExits(): void {
    if (this.disposed || this.pendingExits.length === 0) return;
    const events = this.pendingExits;
    this.pendingExits = [];
    const wake = formatTaskNotification(events, this.deps.listStillRunning?.() ?? []);
    this.deliver({
      customType: FAMULUS_WAKE_CUSTOM_TYPE,
      content: wake.content,
      details: wake.details,
    });
  }

  private deliver(message: NotifyMessage, passive = false, onDelivered?: MonitorDeliveryAck, sourcePreview?: string): void {
    // Wakes built here (task batches, monitor events) are stamped now.
    message = stampWakeAsOf(message, this.clock.now());
    const msg = { ...message, display: true };
    const details = message.details as { kind?: string; eventCount?: number } | undefined;
    if (details?.kind) {
      const ids = wakeIds(details);
      this.deps.logEvent?.("wake.emit", {
        kind: details.kind,
        ids,
        batch: ids.length > 1 || (details.kind === "monitor" && (details.eventCount ?? 0) > 1),
      });
    }
    const mode = passive ? "passive" : this.deps.isIdle() ? "trigger" : "steer";
    this.deps.logEvent?.("wake.deliver", { kind: details?.kind ?? "unknown", mode });
    if (mode !== "passive") this.runPending = true;
    if (mode === "passive") {
      this.deps.sendMessage(msg, { triggerTurn: false });
    } else if (mode === "trigger") {
      this.clearHang();
      this.deps.sendMessage(msg, { triggerTurn: true });
    } else {
      this.deps.sendMessage(msg, { deliverAs: "steer" });
      this.armHang(message);
    }
    onDelivered?.(sourcePreview ?? (details?.kind === "monitor" ? (message.details as { event?: string }).event : undefined));
  }

  private armHang(message: NotifyMessage): void {
    this.steered = message;
    if (this.hangTimer !== null) return;
    if (!(this.hangMs > 0)) return;
    this.hangTimer = this.clock.setTimeout(() => this.onHang(), this.hangMs);
  }

  private clearHang(): void {
    if (this.hangTimer !== null) {
      this.clock.clearTimeout(this.hangTimer);
      this.hangTimer = null;
    }
    this.steered = null;
  }

  private onHang(): void {
    this.hangTimer = null;
    const steered = this.steered;
    this.steered = null;
    if (this.disposed || this.deps.isIdle() || !steered) return;
    const hang: NotifyMessage = {
      customType: FAMULUS_WAKE_CUSTOM_TYPE,
      content:
        "System wake — not a new user message.\n\n" +
        "The parent model stream has produced no tokens. A queued wake follows as a new turn.",
      details: { kind: "parent-hang" },
    };
    this.deps.logEvent?.("wake.deliver", { kind: "parent-hang", mode: "trigger" });
    this.runPending = true;
    this.deps.sendMessage({ ...stampWakeAsOf(hang, this.clock.now()), display: true }, { triggerTurn: true });
    this.deps.logEvent?.("wake.deliver", { kind: (steered.details as { kind?: string } | undefined)?.kind ?? "unknown", mode: "trigger" });
    this.deps.sendMessage({ ...stampWakeAsOf(steered, this.clock.now()), display: true }, { triggerTurn: true });
  }
}
