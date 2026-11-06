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
import { formatExitFacts, formatMonitorEvent, formatMonitorTerminal, formatRunningMonitorList, formatTaskNotification, type MonitorTerminalStatus, type TaskExitInfo } from "./format";
import { FAMULUS_WAKE_CUSTOM_TYPE, stampWakeAsOf, wakeIds, type MonitorOutputInfo, type WakeItem } from "./wake";
import { realClock, type Clock, type ClockTimer } from "./clock";

/**
 * Fixed, non-resetting window that coalesces timeout/exited/stopped monitor notices.
 * Nearby expiries share one wake for completeness. A busy agent is already turn-bound,
 * so this adds no extra delay before the next turn. An idle agent with a single exit
 * may wake up to 10s later. The window starts on the first notice and does not reset;
 * NotifyCenter.flush() and session teardown release pending terminals immediately.
 */
export const MONITOR_TERMINAL_BATCH_MS = 10_000;

const TERMINAL_STATUS_RANK: Record<MonitorTerminalStatus, number> = {
  exited: 1,
  stopped: 2,
  timeout: 3,
};


type MonitorDeliveryAck = (visiblePreview?: string) => void;

const MONITOR_TERMINAL_STATUSES = new Set<string>(["timeout", "exited", "stopped"]);

function isMonitorTerminalStatus(status: string | undefined): status is MonitorTerminalStatus {
  return status !== undefined && MONITOR_TERMINAL_STATUSES.has(status);
}

interface PendingMonitorTerminal {
  id: string;
  description: string;
  status: MonitorTerminalStatus;
  event: string;
  droppedLines?: number;
  output?: MonitorOutputInfo;
  exitFacts?: import("./wake").MonitorExitFacts;
  exitContext?: string;
  passive: boolean;
}

function mergeTerminal(previous: PendingMonitorTerminal, next: PendingMonitorTerminal, onConflict: (fields: string[]) => void): PendingMonitorTerminal {
  const status =
    TERMINAL_STATUS_RANK[next.status] > TERMINAL_STATUS_RANK[previous.status] ? next.status : previous.status;
  const hasFacts = (facts: PendingMonitorTerminal["exitFacts"]) => !!facts &&
    (facts.endReason !== undefined || facts.signal !== undefined || facts.exitCode !== undefined || facts.stopRequested === true);
  const previousKnown = hasFacts(previous.exitFacts);
  const nextKnown = hasFacts(next.exitFacts);
  const chosen = previousKnown ? previous : nextKnown ? next : next.event.length > previous.event.length ? next : previous;
  const other = chosen === next ? previous : next;
  const a = chosen.exitFacts;
  const b = other.exitFacts;
  const conflicts: string[] = [];
  if (a && b) {
    if (a.endReason !== undefined && b.endReason !== undefined && a.endReason !== b.endReason) conflicts.push("endReason");
    if (a.signal !== undefined && b.signal !== undefined && a.signal !== b.signal) conflicts.push("signal");
    if (a.exitCode != null && b.exitCode != null && a.exitCode !== b.exitCode) conflicts.push("exitCode");
  }
  const conflict = conflicts.length > 0;
  if (conflict) onConflict(conflicts);
  const exitFacts = a && b && !conflict ? {
    ...b,
    ...a,
    ...(a.endReason === undefined && b.endReason !== undefined ? { endReason: b.endReason } : {}),
    ...(a.signal === undefined && b.signal !== undefined ? { signal: b.signal } : {}),
    ...(a.exitCode == null && b.exitCode != null ? { exitCode: b.exitCode } : {}),
    ...(a.stopRequested === true || b.stopRequested === true ? { stopRequested: true } : {}),
  } : a;
  const exitContext = chosen.exitContext ?? (!conflict ? other.exitContext : undefined);
  const event = previousKnown && nextKnown && !conflict && exitFacts
    ? `${formatExitFacts(exitFacts)}${exitContext ? ` ${exitContext}` : ""}`
    : chosen.event;
  const description = next.description.length > previous.description.length ? next.description : previous.description;
  const droppedLines = Math.max(previous.droppedLines ?? 0, next.droppedLines ?? 0);
  return {
    id: previous.id,
    description,
    status,
    event,
    ...(droppedLines > 0 ? { droppedLines } : {}),
    output: mergeMonitorOutput(previous.output, next.output),
    exitFacts,
    exitContext,
    passive: previous.passive && next.passive,
  };
}

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
  private pendingTerminals: PendingMonitorTerminal[] = [];
  private terminalTimer: ClockTimer | null = null;
  /** Ids already delivered in a monitor-terminal wake; late duplicates do not open another window. */
  private readonly seenTerminalIds = new Set<string>();
  private disposed = false;
  /** A wake started or steered a run that has not settled yet. */
  private runPending = false;
  private holdAll = false;
  private held: { message: NotifyMessage; onDelivered?: MonitorDeliveryAck; sourcePreview?: string; passive?: boolean }[] = [];

  constructor(deps: NotifyCenterDeps) {
    this.deps = deps;
    this.batchMs = deps.batchMs ?? 200;
    this.clock = deps.clock ?? realClock;
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
   * ahead of the lines the command printed. timeout/exited/stopped notices then
   * join one fixed 10s window rather than each starting a turn.
   */
  notify(message: NotifyMessage, opts: { passive?: boolean; quietEvents?: boolean } = {}): void {
    if (this.disposed) return;
    const wake = message.details as {
      kind?: string;
      id?: string;
      description?: string;
      status?: string;
      event?: string;
      droppedLines?: number;
      output?: MonitorOutputInfo;
      exitFacts?: import("./wake").MonitorExitFacts;
      exitContext?: string;
    } | undefined;
    if (wake?.kind === "monitor" && typeof wake.id === "string") {
      this.flushMonitor(wake.id, opts.quietEvents);
      if (isMonitorTerminalStatus(wake.status)) {
        this.queueTerminal({
          id: wake.id,
          description: wake.description ?? wake.id,
          status: wake.status,
          event: wake.event ?? "",
          droppedLines: wake.droppedLines,
          output: wake.output,
          exitFacts: wake.exitFacts,
          exitContext: wake.exitContext,
          passive: opts.passive === true,
        });
        return;
      }
    }
    // Stamped on arrival: a held passive notice keeps the time it was built.
    message = stampWakeAsOf(message, this.clock.now());
    if (!opts.passive) {
      this.deliver(message);
    } else {
      this.deliverPassive(message);
    }
  }

  private deliverPassive(message: NotifyMessage, onDelivered?: MonitorDeliveryAck, sourcePreview?: string): void {
    message = stampWakeAsOf(message, this.clock.now());
    if (this.holdAll || this.runPending || !this.deps.isIdle()) this.held.push({ message, onDelivered, sourcePreview, passive: true });
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
    this.flushMonitorEvents();
    if (this.runPending) return;
    for (const item of this.held.splice(0)) {
      this.deliver(item.message, item.passive === true, item.onDelivered, item.sourcePreview);
    }
  }

  /** Compaction is in progress: do not send; coalesced/held wakes wait for {@link settledAfterCompact}. */
  beginHold(): void {
    if (this.disposed) return;
    this.holdAll = true;
  }

  endHold(): void {
    this.holdAll = false;
  }

  /**
   * After compaction has actually gone idle: drain held/pending wakes and an
   * optional passive running-monitor list. If any active wake exists, append
   * the entire batch before the final message starts one turn.
   */
  settledAfterCompact(extra?: NotifyMessage): void {
    if (this.disposed) return;
    this.holdAll = false;
    this.runPending = false;
    const batch: { message: NotifyMessage; onDelivered?: MonitorDeliveryAck; sourcePreview?: string; passive?: boolean }[] = [];
    for (const taskId of [...this.pendingMonitors.keys()]) {
      const item = this.takeMonitor(taskId);
      if (item) batch.push(item);
    }
    const terminals = this.takePendingTerminals();
    if (terminals.length > 0) {
      const ids = new Set(terminals.map((item) => item.id));
      batch.push(...this.takeHeldMonitorEvents(ids));
      const wake = formatMonitorTerminal(terminals);
      batch.push({
        message: { customType: wake.customType, content: wake.content, details: wake.details },
        passive: terminals.every((item) => item.passive),
      });
    }
    batch.push(...this.held.splice(0));
    if (extra) batch.push({ message: extra, passive: true });
    if (batch.length === 0) return;
    const anyActive = batch.some((item) => !item.passive);
    if (extra && anyActive) {
      const list = batch.pop()!;
      let lastActive = batch.length - 1;
      while (lastActive >= 0 && batch[lastActive].passive) lastActive--;
      batch.splice(lastActive, 0, list);
    }
    for (let i = 0; i < batch.length; i++) {
      this.sendCompactFlush(batch[i], anyActive && i === batch.length - 1);
    }
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
    if (this.deps.isIdle() && !this.holdAll && !pending && !opts.quiet) {
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
    const item = this.takeMonitor(taskId, quiet);
    if (!item) return;
    if (item.passive) this.deliverPassive(item.message, item.onDelivered, item.sourcePreview);
    else this.deliver(item.message, false, item.onDelivered, item.sourcePreview);
  }

  private takeMonitor(taskId: string, quiet = false): { message: NotifyMessage; onDelivered?: MonitorDeliveryAck; sourcePreview?: string; passive?: boolean } | undefined {
    const item = this.pendingMonitors.get(taskId);
    if (!item) return undefined;
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
    return { message, onDelivered: item.onDelivered, sourcePreview, passive: quiet || item.quiet };
  }

  /** Flush any pending task exit notifications and monitor-terminal notices now. */
  flush(): void {
    this.clearTimer();
    this.flushExits();
    this.flushAllTerminals();
    this.flushMonitorEvents();
  }

  /**
   * Take pending monitor-terminal items without sending through pi.
   * File teardown appends each returned message to the old session file instead.
   * Held output for those terminal ids comes first, in held order; the terminal wake is last.
   * Taking a held output message counts as delivery for its acknowledgement.
   */
  drainPendingMonitorTerminals(): NotifyMessage[] {
    const items = this.takePendingTerminals();
    const ids = new Set(items.map((item) => item.id));
    for (const item of this.held) {
      const details = item.message.details as { kind?: string } | undefined;
      if (details?.kind === "monitor-terminal") {
        for (const id of wakeIds(details)) ids.add(id);
      }
    }
    if (items.length === 0 && ids.size === 0) return [];
    const held = this.takeHeldMonitorEvents(ids, true);
    const messages: NotifyMessage[] = [];
    for (const item of held) {
      messages.push(item.message);
      this.acknowledgeHeld(item);
    }
    if (items.length > 0) {
      const wake = formatMonitorTerminal(items);
      messages.push({ customType: wake.customType, content: wake.content, details: wake.details });
    }
    return messages;
  }

  dispose(): void {
    if (this.disposed) return;
    this.clearTimer();
    this.flushAllTerminals();
    const leftover = this.held.splice(0);
    this.disposed = true;
    this.clearTerminalTimer();
    this.pendingExits = [];
    this.pendingMonitors.clear();
    this.pendingTerminals = [];
    for (const { message, onDelivered, sourcePreview } of leftover) {
      this.deliver(message, true, onDelivered, sourcePreview);
    }
  }

  private queueTerminal(item: PendingMonitorTerminal): void {
    if (this.seenTerminalIds.has(item.id)) return;
    const existing = this.pendingTerminals.findIndex((pending) => pending.id === item.id);
    if (existing >= 0) {
      this.pendingTerminals[existing] = mergeTerminal(this.pendingTerminals[existing], item, (fields) => {
        this.deps.logEvent?.("monitor.exit-facts-conflict", { id: item.id, fields });
      });
      return;
    }
    this.pendingTerminals.push(item);
    if (this.terminalTimer !== null) return;
    this.terminalTimer = this.clock.setTimeout(() => {
      this.terminalTimer = null;
      this.flushAllTerminals();
    }, MONITOR_TERMINAL_BATCH_MS);
    this.clock.unref?.(this.terminalTimer);
  }

  private takePendingTerminals(): PendingMonitorTerminal[] {
    this.clearTerminalTimer();
    const items = this.pendingTerminals;
    this.pendingTerminals = [];
    for (const item of items) this.seenTerminalIds.add(item.id);
    return items;
  }

  private takeHeldMonitorEvents(ids: Set<string>, includeTerminals = false): typeof this.held {
    if (this.held.length === 0) return [];
    if (ids.size === 0 && !includeTerminals) return [];
    const rest: typeof this.held = [];
    const drain: typeof this.held = [];
    for (const item of this.held) {
      const details = item.message.details as { kind?: string; id?: string } | undefined;
      if (details?.kind === "monitor" && typeof details.id === "string" && ids.has(details.id)) drain.push(item);
      else if (includeTerminals && details?.kind === "monitor-terminal" && wakeIds(details).some((id) => ids.has(id))) drain.push(item);
      else rest.push(item);
    }
    this.held = rest;
    return drain;
  }

  private acknowledgeHeld(item: (typeof this.held)[number]): void {
    const details = item.message.details as { kind?: string; event?: string } | undefined;
    item.onDelivered?.(item.sourcePreview ?? (details?.kind === "monitor" ? details.event : undefined));
  }

  private drainHeldMonitorEvents(ids: Set<string>): void {
    for (const { message, onDelivered, sourcePreview } of this.takeHeldMonitorEvents(ids)) {
      this.deliver(message, true, onDelivered, sourcePreview);
    }
  }

  private flushAllTerminals(): void {
    const items = this.takePendingTerminals();
    if (items.length === 0) return;
    const ids = new Set(items.map((item) => item.id));
    const allPassive = items.every((item) => item.passive);
    const hold = this.runPending || !this.deps.isIdle();
    const wake = formatMonitorTerminal(items);
    const message = { customType: wake.customType, content: wake.content, details: wake.details };
    if (allPassive && hold) {
      this.deliverPassive(message);
      return;
    }
    this.drainHeldMonitorEvents(ids);
    if (allPassive) this.deliverPassive(message);
    else this.deliver(message);
  }

  private clearTerminalTimer(): void {
    if (this.terminalTimer !== null) {
      this.clock.clearTimeout(this.terminalTimer);
      this.terminalTimer = null;
    }
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

  private sendCompactFlush(
    item: { message: NotifyMessage; onDelivered?: MonitorDeliveryAck; sourcePreview?: string; passive?: boolean },
    triggerTurn: boolean,
  ): void {
    const message = stampWakeAsOf(item.message, this.clock.now());
    const msg = { ...message, display: true };
    const details = message.details as { kind?: string; eventCount?: number; event?: string } | undefined;
    if (details?.kind) {
      const ids = wakeIds(details);
      this.deps.logEvent?.("wake.emit", {
        kind: details.kind,
        ids,
        batch: ids.length > 1 || (details.kind === "monitor" && (details.eventCount ?? 0) > 1),
      });
    }
    const mode = triggerTurn ? "trigger" : item.passive ? "passive" : "append";
    this.deps.logEvent?.("wake.deliver", { kind: details?.kind ?? "unknown", mode });
    if (triggerTurn) this.runPending = true;
    this.deps.sendMessage(msg, { triggerTurn });
    item.onDelivered?.(item.sourcePreview ?? (details?.kind === "monitor" ? details.event : undefined));
  }

  private deliver(message: NotifyMessage, passive = false, onDelivered?: MonitorDeliveryAck, sourcePreview?: string): void {
    // Wakes built here (task batches, monitor events) are stamped now.
    message = stampWakeAsOf(message, this.clock.now());
    if (this.holdAll) {
      this.held.push({ message, onDelivered, sourcePreview, passive });
      return;
    }
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
      this.deps.sendMessage(msg, { triggerTurn: true });
    } else {
      this.deps.sendMessage(msg, { deliverAs: "steer" });
    }
    onDelivered?.(sourcePreview ?? (details?.kind === "monitor" ? (message.details as { event?: string }).event : undefined));
  }

}

/** Next-tick idle check with 25ms retry; new run/compaction callers cancel. */
export function scheduleIdleCheck(opts: {
  isReady: () => boolean;
  onReady: () => void;
  setTimeout?: typeof setTimeout;
  clearTimeout?: typeof clearTimeout;
}): { cancel: () => void } {
  const start = opts.setTimeout ?? setTimeout;
  const stop = opts.clearTimeout ?? clearTimeout;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let cancelled = false;
  const tick = (delayMs: number) => {
    timer = start(() => {
      timer = undefined;
      if (cancelled) return;
      if (!opts.isReady()) {
        tick(25);
        return;
      }
      opts.onReady();
    }, delayMs);
    timer.unref?.();
  };
  tick(0);
  return {
    cancel() {
      cancelled = true;
      if (timer !== undefined) stop(timer);
      timer = undefined;
    },
  };
}

export function attachCompactRecovery(
  pi: { on(event: string, handler: (event: unknown, ctx: { isIdle(): boolean; hasPendingMessages?: () => boolean }) => unknown): unknown },
  deps: {
    getCenter: () => NotifyCenter | null;
    getCtx: () => { isIdle(): boolean; hasPendingMessages?: () => boolean } | null;
    listRunningMonitors: () => { id: string; description: string; remaining: string; wakeOn?: string }[];
    now?: () => number;
    setTimeout?: typeof setTimeout;
    clearTimeout?: typeof clearTimeout;
    idleHoldMs?: number;
    maxHoldMs?: number;
  },
): { dispose: () => void } {
  const idleHoldMs = deps.idleHoldMs ?? 1_000;
  const maxHoldMs = deps.maxHoldMs ?? 600_000;
  const now = deps.now ?? Date.now;
  const startTimer = deps.setTimeout ?? setTimeout;
  const stopTimer = deps.clearTimeout ?? clearTimeout;
  let handle: { cancel: () => void } | undefined;
  let watchdog: { cancel: () => void } | undefined;
  const cancel = () => {
    handle?.cancel();
    handle = undefined;
    watchdog?.cancel();
    watchdog = undefined;
  };
  const flushHeld = () => {
    cancel();
    const center = deps.getCenter();
    if (!center) return;
    const running = deps.listRunningMonitors();
    const extra = running.length > 0 ? formatRunningMonitorList(running) : undefined;
    center.settledAfterCompact(extra);
  };
  const startWatchdog = () => {
    watchdog?.cancel();
    const holdStarted = now();
    let idleSince: number | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let cancelled = false;
    const tick = () => {
      timer = startTimer(() => {
        timer = undefined;
        if (cancelled) return;
        const t = now();
        if (t - holdStarted >= maxHoldMs) {
          flushHeld();
          return;
        }
        const ctx = deps.getCtx();
        const idle = !!ctx && ctx.isIdle() && ctx.hasPendingMessages?.() !== true;
        if (idle) {
          idleSince ??= t;
          if (t - idleSince >= idleHoldMs) {
            flushHeld();
            return;
          }
        } else {
          idleSince = undefined;
        }
        tick();
      }, 25);
      timer.unref?.();
    };
    tick();
    watchdog = {
      cancel() {
        cancelled = true;
        if (timer !== undefined) stopTimer(timer);
        timer = undefined;
      },
    };
  };
  const afterCompact = () => {
    cancel();
    handle = scheduleIdleCheck({
      isReady: () => {
        const ctx = deps.getCtx();
        return !!ctx && ctx.isIdle() && ctx.hasPendingMessages?.() !== true;
      },
      onReady: () => {
        handle = undefined;
        flushHeld();
      },
      setTimeout: startTimer,
      clearTimeout: stopTimer,
    });
  };
  pi.on("session_before_compact", () => {
    cancel();
    deps.getCenter()?.beginHold();
    startWatchdog();
  });
  pi.on("session_compact", afterCompact);
  pi.on("session_compact_failed", afterCompact);
  pi.on("before_agent_start", () => {
    cancel();
    deps.getCenter()?.endHold();
  });
  pi.on("agent_start", () => {
    cancel();
    deps.getCenter()?.endHold();
  });
  return {
    dispose() {
      cancel();
      deps.getCenter()?.endHold();
    },
  };
}
