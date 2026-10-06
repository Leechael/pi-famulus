/**
 * Pure formatting helpers (design doc Appendix A).
 *
 * These functions are the shared contract between implementation and tests.
 * Signatures must match Appendix A exactly.
 */
import {
  formatFamulusWake,
  shellWakeTitle,
  type FamulusWake,
  wakeDuration,
  type FormattedWake,
  type OverrunShell,
  type SubagentOverrunChild,
  type WakeItem,
} from "./wake";

export { FAMULUS_WAKE_LEAD_IN } from "./wake";
export type { FormattedWake, WakeItem } from "./wake";

export interface TruncationInfo {
  truncated: boolean;
  totalLines: number;
  totalBytes: number;
}

function byteLength(text: string): number {
  return Buffer.byteLength(text, "utf8");
}

/**
 * Keep the tail of `text`, bounded by both maxLines and maxBytes
 * (defaults match the built-in bash tool: 2000 lines / 50KB).
 * Never splits a line, except when the final line alone exceeds maxBytes,
 * in which case its byte-tail is kept (UTF-8 lossy).
 */
export function truncateTail(
  text: string,
  maxLines: number = 2000,
  maxBytes: number = 51200,
): { text: string } & TruncationInfo {
  const totalBytes = byteLength(text);
  const lines = text.split("\n");
  const totalLines = lines.length;
  if (totalLines <= maxLines && totalBytes <= maxBytes) {
    return { text, truncated: false, totalLines, totalBytes };
  }
  const kept: string[] = [];
  let bytes = 0;
  for (let i = lines.length - 1; i >= 0; i--) {
    const lineBytes = byteLength(lines[i]) + (kept.length > 0 ? 1 : 0); // +1 for the joining newline
    if (kept.length >= maxLines || bytes + lineBytes > maxBytes) break;
    kept.unshift(lines[i]);
    bytes += lineBytes;
  }
  if (kept.length === 0 && lines.length > 0) {
    // The final line alone exceeds maxBytes: keep its byte tail, aligned to a
    // UTF-8 code point boundary so no replacement character is surfaced.
    kept.push(byteTail(lines[lines.length - 1], maxBytes));
  }
  return { text: kept.join("\n"), truncated: true, totalLines, totalBytes };
}

/** Last `maxBytes` bytes of `text`, cut at a UTF-8 boundary (never mid-character). */
function byteTail(text: string, maxBytes: number): string {
  const buf = Buffer.from(text, "utf8");
  if (buf.length <= maxBytes) return text;
  let start = buf.length - maxBytes;
  // Skip continuation bytes (0b10xxxxxx) to land on a character boundary.
  while (start < buf.length && (buf[start] & 0b1100_0000) === 0b1000_0000) start++;
  return buf.subarray(start).toString("utf8");
}

export interface TaskExitInfo {
  taskId: string;
  kind: string;
  command: string;
  status: "completed" | "failed" | "killed" | "orphaned";
  exitCode: number | null;
  durationMs: number;
  outputPath: string;
  /** Pre-truncated by the caller to 4000 chars. */
  preview: string;
  /** Signal name from the manager, when the exit was a signal. */
  signal?: string;
}

/** Shorten a command for one-line display inside notifications. */
function displayCommand(command: string): string {
  return shellWakeTitle(command);
}

function statusPhrase(info: TaskExitInfo): string {
  const code = info.exitCode === null ? null : `exit code ${info.exitCode}`;
  switch (info.status) {
    case "completed":
      return code ? `completed (${code})` : "completed";
    case "failed":
      return code ? `failed (${code})` : "failed";
    case "killed":
      return "was killed";
    case "orphaned":
      return "ended when pi-famulus crashed";
  }
}

const TASK_COMMAND_CHARS = 2000;

function capCommand(command: string): string {
  return command.length > TASK_COMMAND_CHARS ? `${command.slice(0, TASK_COMMAND_CHARS)}…` : command;
}

function taskSummary(info: TaskExitInfo): string {
  return `Background command "${displayCommand(info.command)}" ${statusPhrase(info)}`;
}

/**
 * One <pi-famulus-wake kind="task"> for a batch of exits (§4.5).
 * `leadIn` is the ablation seam: "" leaves a parseable envelope.
 */
export function formatTaskNotification(
  events: TaskExitInfo[],
  stillRunning: WakeItem[] = [],
  leadIn?: string,
): FormattedWake {
  return formatFamulusWake(
    {
      kind: "task",
      stillRunning,
      tasks: events.map((info) => ({
        id: info.taskId,
        taskKind: info.kind,
        status: info.status,
        summary: taskSummary(info),
        command: capCommand(info.command),
        outputPath: info.outputPath,
        preview: info.preview,
        durationMs: info.durationMs,
        exitCode: info.exitCode,
        ...(info.signal ? { signal: info.signal } : {}),
      })),
    },
    leadIn,
  );
}

/** Tool-result text returned when a foreground command is moved to the background (§4.2). */
export function formatBackgroundNotice(
  taskId: string,
  command: string,
  outputPath: string,
): string {
  // Model-facing. The transcript row is drawn by the bash tool's renderResult.
  return [
    `Command "${displayCommand(command)}" moved to background (task_id: ${taskId}). Output: ${outputPath}.`,
    'You will be notified when it completes, even if other commands are still running. Do not poll or sleep: reply to the user now with no tool call, and continue from the <pi-famulus-wake kind="task"> when it arrives.',
  ].join("\n");
}

/** Transcript row for a backgrounded command (UI only; never sent to the model). */
export function backgroundRowText(
  taskId: string,
  item: { status: string; exitCode?: number | null; startedAt: number; endedAt?: number } | undefined,
  now: number,
): { glyph: string; color: string; text: string } {
  if (!item || item.status === "running" || item.status === "pending") {
    return { glyph: "⏵", color: "accent", text: `${taskId} running in background · /tasks` };
  }
  const secs = ((item.endedAt ?? now) - item.startedAt) / 1000;
  const dur = secs < 60 ? `${secs.toFixed(1)}s` : `${Math.floor(secs / 60)}m${Math.round(secs % 60)}s`;
  const exit = item.exitCode === null || item.exitCode === undefined ? "" : ` · exit ${item.exitCode}`;
  const ok = item.status === "completed" && (item.exitCode === 0 || item.exitCode === null || item.exitCode === undefined);
  return ok
    ? { glyph: "✓", color: "success", text: `${taskId} finished${exit} · ${dur}` }
    : { glyph: "✗", color: "error", text: `${taskId} ${item.status}${exit} · ${dur} · /tasks` };
}

/** Injected payload for a batch of monitor output lines (§4.4 / §4.5). */
export function formatMonitorEvent(
  description: string,
  taskId: string,
  batchText: string,
  status?: string,
  extras: { eventCount?: number; droppedLines?: number } = {},
): FormattedWake {
  return formatFamulusWake({
    kind: "monitor",
    id: taskId,
    description,
    ...(status ? { status } : {}),
    event: batchText,
    ...(extras.eventCount !== undefined ? { eventCount: extras.eventCount } : {}),
    ...(extras.droppedLines !== undefined ? { droppedLines: extras.droppedLines } : {}),
  });
}

// ---------------------------------------------------------------------------
// M3: subagent run completion notification (design doc §4.6)
// ---------------------------------------------------------------------------

export interface SubagentChildInfo {
  childId: string;
  name: string;
  status: "pending" | "running" | "completed" | "failed" | "interrupted";
  text: string;
  error?: string;
  /** Non-fatal caveat from the child run (e.g. an adapted model spec). */
  warning?: string;
  /** Task prompt that was sent to this child. */
  prompt?: string;
}

export interface SubagentNotificationInfo {
  runId: string;
  status: "completed" | "partial" | "failed" | "interrupted";
  durationMs: number;
  children: SubagentChildInfo[];
}

/** Per-child result text is capped to its tail inside notifications (§4.6). */
const SUBAGENT_NOTIFICATION_CHILD_CHARS = 2000;

function capTail(text: string): string {
  return text.length > SUBAGENT_NOTIFICATION_CHILD_CHARS
    ? text.slice(-SUBAGENT_NOTIFICATION_CHILD_CHARS)
    : text;
}

/** Keep the start of a task prompt; the assignment is at the beginning. */
function capPrompt(text: string): string {
  return text.length > SUBAGENT_NOTIFICATION_CHILD_CHARS
    ? `${text.slice(0, SUBAGENT_NOTIFICATION_CHILD_CHARS)}…`
    : text;
}

export interface SubagentHandoverInfo {
  runId: string;
  childId: string;
  name: string;
  status: "completed" | "failed" | "interrupted";
  prompt: string;
  text: string;
  error?: string;
  warning?: string;
  /** Children that are still pending or running. Title is the agent name. */
  stillRunning: WakeItem[];
}

/** Finished run: one <child> per child inside <pi-famulus-wake kind="subagent-done">. */
export function formatSubagentNotification(info: SubagentNotificationInfo): FormattedWake {
  const completed = info.children.filter((c) => c.status === "completed").length;
  const summary = `${completed}/${info.children.length} subagents completed in ${Math.round(info.durationMs)}ms`;
  return formatFamulusWake({
    kind: "subagent-done",
    runId: info.runId,
    status: info.status,
    durationMs: info.durationMs,
    summary,
    children: info.children.map((child) => ({
      childId: child.childId,
      name: child.name,
      status: child.status,
      prompt: capPrompt(child.prompt ?? ""),
      result: capTail(child.text),
      ...(child.error ? { error: child.error } : {}),
      ...(child.warning ? { warning: child.warning } : {}),
    })),
  });
}

/**
 * One child finished while others in the same run are still going.
 * The parent must see the original prompt and the result, then keep working.
 */
export function formatSubagentHandover(info: SubagentHandoverInfo): FormattedWake {
  const summary = `${info.name} ${info.status}; ${info.stillRunning.length} still running`;
  return formatFamulusWake({
    kind: "subagent-handover",
    runId: info.runId,
    childId: info.childId,
    name: info.name,
    status: info.status,
    stillRunning: info.stillRunning,
    summary,
    prompt: capPrompt(info.prompt),
    result: capTail(info.text),
    ...(info.error ? { error: info.error } : {}),
    ...(info.warning ? { warning: info.warning } : {}),
  });
}

// ---------------------------------------------------------------------------
// Soft deadline: a running child passed its budget (design doc §4.6 lifecycle)
// ---------------------------------------------------------------------------

export interface SubagentOverrunInfo {
  runId: string;
  childId: string;
  name: string;
  elapsedMs: number;
  budgetMs: number;
  reminder: number;
  nextReminderMs: number;
  /** Time left until the opt-in hard ceiling aborts the child. Absent when none is set. */
  hardCeilingMs?: number;
  lastActivity: { agoMs: number; text: string };
  shell?: OverrunShell;
}

/** Last-activity text cap: enough to see what the child was doing. */
const OVERRUN_ACTIVITY_CHARS = 300;

/**
 * A child is past its soft budget and still running. The parent decides:
 * extend, steer, or interrupt; doing nothing leaves it running.
 */
function formatOverrunChild(info: SubagentOverrunInfo): SubagentOverrunChild {
  const oneLine = info.lastActivity.text.replace(/\s+/g, " ").trim();
  const text = oneLine.length > OVERRUN_ACTIVITY_CHARS ? `…${oneLine.slice(-OVERRUN_ACTIVITY_CHARS)}` : oneLine;
  return {
    runId: info.runId,
    childId: info.childId,
    name: info.name,
    elapsedMs: info.elapsedMs,
    budgetMs: info.budgetMs,
    reminder: info.reminder,
    nextReminderMs: info.nextReminderMs,
    ...(info.hardCeilingMs !== undefined ? { hardCeilingMs: info.hardCeilingMs } : {}),
    lastActivity: { agoMs: info.lastActivity.agoMs, text: text || "(no output yet)" },
    ...(info.shell ? { shell: { ...info.shell, command: shellWakeTitle(info.shell.command) } } : {}),
  };
}

function formatSubagentOverrunDetails(info: SubagentOverrunInfo): Extract<FamulusWake, { kind: "subagent-overrun" }> {
  const child = formatOverrunChild(info);
  const shellBit = child.shell ? `; it is waiting on a shell command that has run ${wakeDuration(child.shell.elapsedMs)}` : "";
  const summary =
    `${child.name} has run ${wakeDuration(child.elapsedMs)} in this turn, past its ` +
    `${wakeDuration(child.budgetMs)} budget, and is still running${shellBit}.`;
  return { kind: "subagent-overrun", ...child, summary };
}

export function formatSubagentOverrun(info: SubagentOverrunInfo): FormattedWake {
  return formatFamulusWake(formatSubagentOverrunDetails(info));
}

export function formatSubagentOverrunBatch(infos: SubagentOverrunInfo[]): FormattedWake {
  if (infos.length === 1) return formatSubagentOverrun(infos[0]);
  return formatFamulusWake({
    ...formatSubagentOverrunDetails(infos[0]),
    additional: infos.slice(1).map(formatOverrunChild),
  });
}
