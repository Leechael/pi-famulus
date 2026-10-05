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

export interface SubagentDoneChild {
  childId: string;
  name: string;
  status: "pending" | "running" | "completed" | "failed" | "interrupted";
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

export type FamulusWake =
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
      status: "completed" | "failed" | "interrupted";
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

function renderWake(details: FamulusWake): string {
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
  const parts = [`<pi-famulus-wake ${attrs.join(" ")}>`];
  const still = stillRunningXml(details.stillRunning);
  if (still) parts.push(still);
  parts.push(`  <summary>${escapeXml(details.summary)}</summary>`);
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
  const parts = [`<pi-famulus-wake ${attrs.join(" ")}>`, `  <summary>${escapeXml(details.summary)}</summary>`];
  for (const child of details.children) {
    parts.push(
      `  <child id="${escapeXmlAttr(child.childId)}" name="${escapeXmlAttr(child.name)}" status="${escapeXmlAttr(child.status)}">`,
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
      `If you do none of these, it keeps running and this reminder arrives again in ${wakeDuration(details.nextReminderMs)}; ` +
      "its result arrives as usual when it finishes."
    );
  }
  // Opt-in hard ceiling: never promise "keeps running" past it.
  const ceiling = wakeDuration(hard);
  const noAction =
    details.nextReminderMs < hard
      ? `If you do none of these, it keeps running and this reminder arrives again in ${wakeDuration(details.nextReminderMs)}, until the hard ceiling stops it in ${ceiling}.`
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
