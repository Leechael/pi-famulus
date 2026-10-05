/**
 * Unified wake envelope (design doc §4.5).
 *
 * One customType, one lead-in, one <pi-famulus-wake> element. details.kind is the
 * discriminator the renderer and the eval adapter both read.
 */

export const FAMULUS_WAKE_CUSTOM_TYPE = "pi-famulus-wake";

export const FAMULUS_WAKE_LEAD_IN =
  "System wake — not a new user message. Handle this <pi-famulus-wake> before other work.";

export interface WakeItem {
  id: string;
  title: string;
}

export interface TaskWake {
  id: string;
  taskKind: string;
  status: "completed" | "failed" | "killed" | "orphaned";
  summary: string;
  command: string;
  outputPath: string;
  preview: string;
  durationMs: number;
  exitCode: number | null;
  /** Signal name ("SIGTERM" | "SIGKILL"). Omitted when the exit had no signal. */
  signal?: string;
}

export type WakeChildStatus = "pending" | "running" | "completed" | "failed" | "interrupted";

export interface SubagentDoneChild {
  childId: string;
  name: string;
  /** Status when the model sees the wake (re-checked at injection). */
  status: WakeChildStatus;
  /** Set when the status changed after as-of: the status in the snapshot. */
  statusAsOf?: WakeChildStatus;
  prompt: string;
  result: string;
  error?: string;
  /** Non-fatal caveat from the child run (e.g. an adapted model spec). */
  warning?: string;
}

/** The foreground shell a child is blocked on when its overrun wake is built. */
export interface OverrunShell {
  taskId: string;
  /** One line, capped (shellWakeTitle). */
  command: string;
  elapsedMs: number;
  outputPath: string;
  /** Output file size; null when the file could not be read. */
  outputBytes: number | null;
  /** Time since the output file last changed; null when unknown. */
  outputIdleMs: number | null;
  /** Output grew since the previous reminder, or changed in the last minute; null when unknown. */
  growing: boolean | null;
}

/**
 * When the wake's content was generated (design.md §4.5): stamped by the
 * NotifyCenter when it receives or builds the wake. A steered wake can wait
 * minutes before the model sees it; the model needs to know how old the
 * snapshot is.
 */
export interface WakeTiming {
  /** Epoch ms. Rendered as the root attribute as-of (UTC, seconds). */
  asOf?: number;
  /**
   * How old the wake was when it entered the model's context (ms). Set at
   * injection (wake-delivery.ts), rendered as the root attribute age-ms.
   */
  ageMs?: number;
}

export type FamulusWake = FamulusWakeBody & WakeTiming;

type FamulusWakeBody =
  | { kind: "task"; stillRunning: WakeItem[]; tasks: TaskWake[] }
  | {
      kind: "monitor";
      id: string;
      description: string;
      status?: string;
      event: string;
      eventCount?: number;
      droppedLines?: number;
    }
  | {
      kind: "subagent-handover";
      runId: string;
      childId: string;
      name: string;
      /** The settle this wake reports; pending/running only when re-checked at injection. */
      status: WakeChildStatus;
      /** Set when the status changed after as-of: the status in the snapshot. */
      statusAsOf?: WakeChildStatus;
      stillRunning: WakeItem[];
      summary: string;
      prompt: string;
      result: string;
      error?: string;
      warning?: string;
    }
  | {
      kind: "subagent-done";
      runId: string;
      status: "completed" | "partial" | "failed" | "interrupted";
      /** Set at injection when a child is pending or running again: the run's status then. */
      runStatusNow?: string;
      durationMs: number;
      summary: string;
      children: SubagentDoneChild[];
    }
  | {
      kind: "subagent-overrun";
      runId: string;
      childId: string;
      name: string;
      /** Time in this user turn (since admission of the launch or resume). */
      elapsedMs: number;
      /** The turn's soft budget (timeout_ms), extensions included. */
      budgetMs: number;
      /** 1 for the first wake of this turn, then 2, 3, … */
      reminder: number;
      /** When the next reminder is due if the parent does nothing. */
      nextReminderMs: number;
      /** Time left until the opt-in hard ceiling aborts the child; absent when none is set. */
      hardCeilingMs?: number;
      summary: string;
      lastActivity: { agoMs: number; text: string };
      shell?: OverrunShell;
    }
  | { kind: "supervisor-request"; from: string; name: string; message: string }
  | { kind: "supervisor-update"; from: string; name: string; message: string };

export interface FormattedWake {
  customType: typeof FAMULUS_WAKE_CUSTOM_TYPE;
  content: string;
  details: FamulusWake;
}

export function escapeXml(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

export function escapeXmlAttr(text: string): string {
  return escapeXml(text).replace(/"/g, "&quot;");
}

/** Shell still-running title: one line, capped at 80 characters. */
export function shellWakeTitle(command: string): string {
  const one = command.replace(/\s+/g, " ").trim();
  if (one.length <= 80) return one;
  return `${one.slice(0, 79)}…`;
}

export function formatFamulusWake(details: FamulusWake, leadIn: string = FAMULUS_WAKE_LEAD_IN): FormattedWake {
  const xml = renderWake(details);
  const content = leadIn ? `${leadIn}\n\n${xml}` : xml;
  return { customType: FAMULUS_WAKE_CUSTOM_TYPE, content, details };
}

/** Task/child/monitor ids a wake is about (event log `ids`). */
export function wakeIds(details: { kind?: string }): string[] {
  const d = details as {
    kind?: string;
    id?: string;
    taskId?: string;
    tasks?: { id: string }[];
    children?: { childId: string }[];
    childId?: string;
    from?: string;
  };
  if (d.kind === "task") return (d.tasks ?? []).map((task) => task.id);
  if (d.kind === "subagent-done") return (d.children ?? []).map((child) => child.childId);
  return [d.id ?? d.taskId ?? d.childId ?? d.from].filter((id): id is string => Boolean(id));
}

/** UTC, second precision: the model needs how old a snapshot is, not milliseconds. */
export function wakeTimestamp(ms: number): string {
  return new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");
}

/** Append attributes to the root <pi-famulus-wake …> tag of rendered content. */
function addRootAttrs(content: string, attrs: string[]): string {
  if (attrs.length === 0) return content;
  // `\s`: the lead-in mentions a bare <pi-famulus-wake>; the root tag has attributes.
  const root = /<pi-famulus-wake\s[^>]*>/.exec(content);
  if (!root) return content;
  const end = root.index + root[0].length - 1;
  return `${content.slice(0, end)} ${attrs.join(" ")}${content.slice(end)}`;
}

function timingAttrs(details: WakeTiming): string[] {
  const attrs: string[] = [];
  if (details.asOf !== undefined) attrs.push(`as-of="${wakeTimestamp(details.asOf)}"`);
  if (details.ageMs !== undefined) attrs.push(`age-ms="${Math.round(details.ageMs)}"`);
  return attrs;
}

/** The <pi-famulus-wake> element for `details` (no lead-in). */
export function renderWakeXml(details: FamulusWake): string {
  return renderWake(details);
}

/** Index of the root <pi-famulus-wake …> tag in content, or -1. */
export function wakeRootIndex(content: string): number {
  return content.search(/<pi-famulus-wake\s/);
}

/**
 * Stamp a wake with the time it was generated. Leaves non-wake messages and
 * already-stamped wakes alone, so a wake held for later delivery keeps the
 * time it was built.
 */
export function stampWakeAsOf<M extends { customType: string; content: string; details?: unknown }>(
  message: M,
  asOf: number,
): M {
  if (message.customType !== FAMULUS_WAKE_CUSTOM_TYPE) return message;
  const details = message.details as (WakeTiming & { kind?: string }) | undefined;
  if (!details || typeof details !== "object" || !details.kind || details.asOf !== undefined) return message;
  const stamped = { ...details, asOf };
  const content = addRootAttrs(message.content, timingAttrs(stamped));
  // addRootAttrs is a no-op when the root tag is missing; keep details in
  // sync with content so wakeAtInjection does not reject a phantom stamp.
  if (content === message.content) return message;
  return { ...message, content, details: stamped };
}

function renderWake(details: FamulusWake): string {
  return addRootAttrs(renderWakeBody(details), timingAttrs(details));
}

function renderWakeBody(details: FamulusWake): string {
  switch (details.kind) {
    case "task":
      return renderTask(details);
    case "monitor":
      return renderMonitor(details);
    case "subagent-handover":
      return renderHandover(details);
    case "subagent-done":
      return renderDone(details);
    case "subagent-overrun":
      return renderOverrun(details);
    case "supervisor-request":
      return renderRequest(details);
    case "supervisor-update":
      return renderUpdate(details);
  }
}

function stillRunningXml(items: WakeItem[]): string {
  if (items.length === 0) return "";
  const lines = items.map((item) => `    <item id="${escapeXmlAttr(item.id)}">${escapeXml(item.title)}</item>`);
  return ["  <still-running>", ...lines, "  </still-running>"].join("\n");
}

function renderTask(details: Extract<FamulusWake, { kind: "task" }>): string {
  const parts = ['<pi-famulus-wake kind="task">'];
  const still = stillRunningXml(details.stillRunning);
  if (still) parts.push(still);
  for (const task of details.tasks) {
    const attrs = [
      `id="${escapeXmlAttr(task.id)}"`,
      `kind="${escapeXmlAttr(task.taskKind)}"`,
      `status="${escapeXmlAttr(task.status)}"`,
      `duration-ms="${Math.round(task.durationMs)}"`,
    ];
    if (task.exitCode !== null) attrs.push(`exit-code="${task.exitCode}"`);
    if (task.signal) attrs.push(`signal="${escapeXmlAttr(task.signal)}"`);
    parts.push(`  <task ${attrs.join(" ")}>`);
    parts.push(`    <summary>${escapeXml(task.summary)}</summary>`);
    parts.push(`    <command>${escapeXml(task.command)}</command>`);
    parts.push(`    <output-file>${escapeXml(task.outputPath)}</output-file>`);
    parts.push(`    <preview>${escapeXml(task.preview)}</preview>`);
    parts.push("  </task>");
  }
  parts.push("</pi-famulus-wake>");
  return parts.join("\n");
}

function renderMonitor(details: Extract<FamulusWake, { kind: "monitor" }>): string {
  const attrs = [
    'kind="monitor"',
    `id="${escapeXmlAttr(details.id)}"`,
    `description="${escapeXmlAttr(details.description)}"`,
  ];
  if (details.status) attrs.push(`status="${escapeXmlAttr(details.status)}"`);
  if (details.eventCount !== undefined && details.eventCount > 1) attrs.push(`event-count="${details.eventCount}"`);
  if (details.droppedLines !== undefined && details.droppedLines > 0) {
    attrs.push(`dropped-lines="${details.droppedLines}"`);
  }
  return [`<pi-famulus-wake ${attrs.join(" ")}>`, `  <event>${escapeXml(details.event)}</event>`, "</pi-famulus-wake>"].join("\n");
}

function renderHandover(details: Extract<FamulusWake, { kind: "subagent-handover" }>): string {
  const attrs = [
    'kind="subagent-handover"',
    `run-id="${escapeXmlAttr(details.runId)}"`,
    `child-id="${escapeXmlAttr(details.childId)}"`,
    `name="${escapeXmlAttr(details.name)}"`,
    `status="${escapeXmlAttr(details.status)}"`,
  ];
  if (details.statusAsOf) attrs.push(`status-as-of="${escapeXmlAttr(details.statusAsOf)}"`);
  const parts = [`<pi-famulus-wake ${attrs.join(" ")}>`];
  const still = stillRunningXml(details.stillRunning);
  if (still) parts.push(still);
  parts.push(`  <summary>${escapeXml(details.summary)}</summary>`);
  if (details.statusAsOf) {
    const active = details.status === "pending" || details.status === "running";
    parts.push(
      `  <changed-since-as-of>${escapeXml(
        `${details.name} (${details.childId}): ${details.statusAsOf} → ${details.status}.` +
          (active ? " Its result arrives as a new wake when it finishes." : ""),
      )}</changed-since-as-of>`,
    );
  }
  parts.push(`  <prompt>${escapeXml(details.prompt)}</prompt>`);
  if (details.error) parts.push(`  <error>${escapeXml(details.error)}</error>`);
  if (details.warning) parts.push(`  <warning>${escapeXml(details.warning)}</warning>`);
  parts.push(`  <result>${escapeXml(details.result)}</result>`);
  parts.push("</pi-famulus-wake>");
  return parts.join("\n");
}

function renderDone(details: Extract<FamulusWake, { kind: "subagent-done" }>): string {
  const attrs = [
    'kind="subagent-done"',
    `run-id="${escapeXmlAttr(details.runId)}"`,
    `status="${escapeXmlAttr(details.status)}"`,
    `duration-ms="${Math.round(details.durationMs)}"`,
  ];
  if (details.runStatusNow) attrs.push(`status-now="${escapeXmlAttr(details.runStatusNow)}"`);
  const parts = [`<pi-famulus-wake ${attrs.join(" ")}>`, `  <summary>${escapeXml(details.summary)}</summary>`];
  const changed = details.children.filter((child) => child.statusAsOf !== undefined);
  if (changed.length > 0) {
    const list = changed.map((c) => `${c.name} (${c.childId}): ${c.statusAsOf} → ${c.status}`).join("; ");
    const active = details.children.some((c) => c.status === "pending" || c.status === "running");
    parts.push(
      `  <changed-since-as-of>${escapeXml(
        `${list}.` + (active ? " The run is active again; another subagent-done arrives when it finishes." : ""),
      )}</changed-since-as-of>`,
    );
  }
  for (const child of details.children) {
    const asOf = child.statusAsOf ? ` status-as-of="${escapeXmlAttr(child.statusAsOf)}"` : "";
    parts.push(
      `  <child id="${escapeXmlAttr(child.childId)}" name="${escapeXmlAttr(child.name)}" status="${escapeXmlAttr(child.status)}"${asOf}>`,
    );
    parts.push(`    <prompt>${escapeXml(child.prompt)}</prompt>`);
    if (child.error) parts.push(`    <error>${escapeXml(child.error)}</error>`);
    if (child.warning) parts.push(`    <warning>${escapeXml(child.warning)}</warning>`);
    parts.push(`    <result>${escapeXml(child.result)}</result>`);
    parts.push("  </child>");
  }
  parts.push("</pi-famulus-wake>");
  return parts.join("\n");
}

/** Coarse human duration for model-facing text: "45s", "12m", "1h05m". */
export function wakeDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}m`;
}

function renderOverrun(details: Extract<FamulusWake, { kind: "subagent-overrun" }>): string {
  const attrs = [
    'kind="subagent-overrun"',
    `run-id="${escapeXmlAttr(details.runId)}"`,
    `child-id="${escapeXmlAttr(details.childId)}"`,
    `name="${escapeXmlAttr(details.name)}"`,
    `elapsed-ms="${Math.round(details.elapsedMs)}"`,
    `budget-ms="${Math.round(details.budgetMs)}"`,
    `reminder="${details.reminder}"`,
  ];
  if (details.hardCeilingMs !== undefined) attrs.push(`hard-ceiling-ms="${Math.round(details.hardCeilingMs)}"`);
  const parts = [`<pi-famulus-wake ${attrs.join(" ")}>`, `  <summary>${escapeXml(details.summary)}</summary>`];
  parts.push(
    `  <last-activity ago-ms="${Math.round(details.lastActivity.agoMs)}">${escapeXml(details.lastActivity.text)}</last-activity>`,
  );
  const shell = details.shell;
  if (shell) {
    const shellAttrs = [`task-id="${escapeXmlAttr(shell.taskId)}"`, `elapsed-ms="${Math.round(shell.elapsedMs)}"`];
    if (shell.outputBytes !== null) shellAttrs.push(`output-bytes="${shell.outputBytes}"`);
    if (shell.outputIdleMs !== null) shellAttrs.push(`output-idle-ms="${Math.round(shell.outputIdleMs)}"`);
    if (shell.growing !== null) shellAttrs.push(`growing="${shell.growing ? "yes" : "no"}"`);
    parts.push(`  <shell ${shellAttrs.join(" ")}>`);
    parts.push(`    <command>${escapeXml(shell.command)}</command>`);
    parts.push(`    <output-file>${escapeXml(shell.outputPath)}</output-file>`);
    parts.push("  </shell>");
  }
  parts.push(`  <options>${escapeXml(overrunOptions(details))}</options>`);
  parts.push("</pi-famulus-wake>");
  return parts.join("\n");
}

/** The parent's three actions and what happens if it takes none. */
function overrunOptions(details: Extract<FamulusWake, { kind: "subagent-overrun" }>): string {
  const ids = `run_id: "${details.runId}", child_id: "${details.childId}"`;
  const actions =
    `give it more time with subagent({ action: "extend", ${ids}, timeout_ms: <ms from now> }); ` +
    `redirect it with agent_message({ action: "send", to: "${details.childId}", message: "<instruction>" }); ` +
    `or stop it with subagent({ action: "interrupt", ${ids} }). `;
  const hard = details.hardCeilingMs;
  if (hard === undefined) {
    return (
      "It has not been stopped. Choose one: " +
      actions +
      `If you do none of these, it keeps running and the next reminder is scheduled in ${wakeDuration(details.nextReminderMs)}; ` +
      "its result arrives as usual when it finishes."
    );
  }
  // Opt-in hard ceiling: never promise "keeps running" past it.
  const ceiling = wakeDuration(hard);
  const noAction =
    details.nextReminderMs < hard
      ? `If you do none of these, it keeps running and the next reminder is scheduled in ${wakeDuration(details.nextReminderMs)}, until the hard ceiling stops it in ${ceiling}.`
      : "If you do none of these, it keeps running until the hard ceiling stops it.";
  return (
    `It has not been stopped yet, but the configured hard ceiling stops it in ${ceiling}, and extend does not move that ceiling. ` +
    "Choose one: " +
    actions +
    noAction +
    " Its result, or the interruption, arrives as a wake."
  );
}

function renderRequest(details: Extract<FamulusWake, { kind: "supervisor-request" }>): string {
  const recipe = `agent_message { action: "reply", to: "${details.from}", message: "<your decision>" }`;
  return [
    `<pi-famulus-wake kind="supervisor-request" from="${escapeXmlAttr(details.from)}" name="${escapeXmlAttr(details.name)}">`,
    `  <message>${escapeXml(details.message)}</message>`,
    `  <reply-with>${escapeXml(recipe)}</reply-with>`,
    "</pi-famulus-wake>",
  ].join("\n");
}

function renderUpdate(details: Extract<FamulusWake, { kind: "supervisor-update" }>): string {
  return [
    `<pi-famulus-wake kind="supervisor-update" from="${escapeXmlAttr(details.from)}" name="${escapeXmlAttr(details.name)}">`,
    `  <message>${escapeXml(details.message)}</message>`,
    "</pi-famulus-wake>",
  ].join("\n");
}
