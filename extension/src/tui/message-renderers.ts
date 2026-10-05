/**
 * Custom message renderers for Famulus notifications (Claude-style compact pills).
 *
 * Box is `(paddingX, paddingY, bgFn)` — the second argument is vertical padding,
 * not a child gap. `outputPad` is the horizontal pad (0 or 1), matching pi's
 * custom-message boxes which use `new Box(1, 1, bg)`.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN, type FamulusWake, type TaskWake } from "../wake";
import { fitLines, loadPiTui } from "./pi-tui-load";
import { statusGlyph } from "./tool-component";

type Theme = {
  fg(color: string, text: string): string;
  bg(color: string, text: string): string;
};

type PillComponent = {
  render(width: number): string[];
  invalidate(): void;
};

const STATUS_ORDER = [
  "completed",
  "failed",
  "interrupted",
  "killed",
  "orphaned",
  "pending",
  "running",
  "partial",
  "timeout",
  "stopped",
];

function countStatuses(statuses: string[]): string {
  const counts = new Map<string, number>();
  for (const status of statuses) counts.set(status, (counts.get(status) ?? 0) + 1);
  const parts = STATUS_ORDER.filter((status) => counts.has(status)).map(
    (status) => `${counts.get(status)} ${status}`,
  );
  return parts.join(" · ");
}

function badExit(status: string | undefined, exitCode?: number | null): boolean {
  if (exitCode !== undefined && exitCode !== null && exitCode !== 0) return true;
  return status === "failed" || status === "killed" || status === "orphaned" || status === "interrupted";
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

function taskExit(task: TaskWake): string {
  if (task.exitCode !== null) return `exit ${task.exitCode}`;
  return task.signal ?? task.status;
}

function taskHead(details: Extract<FamulusWake, { kind: "task" }>): string {
  const taskInfo = details.tasks.length === 1
    ? `${formatDuration(details.tasks[0].durationMs)} · ${taskExit(details.tasks[0])}`
    : `${details.tasks.length} tasks · ${countStatuses(details.tasks.map((task) => task.status))}`;
  const still = details.stillRunning.length > 0 ? ` · ${details.stillRunning.length} still running` : "";
  const summary = details.tasks.length === 1 ? details.tasks[0].summary : "Background work";
  return `${summary} · ${taskInfo}${still}`;
}

function collapsedText(details: FamulusWake, theme: Theme): string {
  switch (details.kind) {
    case "task": {
      const bad = details.tasks.some((task) => badExit(task.status, task.exitCode));
      const { color, glyph } = statusGlyph(bad ? "failed" : "completed");
      return `${theme.fg(color, glyph)} ${theme.fg("muted", "task")} ${taskHead(details)}`;
    }
    case "monitor": {
      const preview = details.event.split("\n").find((line) => line.trim().length > 0)?.trim() ?? "(event)";
      const { color, glyph } = details.status
        ? statusGlyph(details.status)
        : { color: "accent", glyph: "›" };
      const statusBit = details.status ? ` ${theme.fg("dim", `· ${details.status}`)}` : "";
      const countBit = details.eventCount && details.eventCount > 1
        ? ` ${theme.fg("dim", `· ${details.eventCount} events`)}`
        : "";
      const droppedBit = details.droppedLines
        ? ` ${theme.fg("warning", `· ${details.droppedLines} lines dropped`)}`
        : "";
      return `${theme.fg(color, glyph)} ${theme.fg("muted", "monitor")} ${theme.fg("accent", `"${details.description}"`)}${statusBit}${countBit}${droppedBit}\n${theme.fg("dim", preview)}`;
    }
    case "subagent-handover": {
      const { color, glyph } = statusGlyph(details.status);
      const snippet = details.result.replace(/\s+/g, " ").trim().slice(0, 72);
      return `${theme.fg(color, glyph)} ${theme.fg("muted", "handover")} ${details.name} ${details.status}${snippet ? ` · ${snippet}` : ""}`;
    }
    case "subagent-done": {
      const bad = details.children.some((child) => badExit(child.status));
      const { color, glyph } = statusGlyph(bad ? "failed" : details.status);
      return `${theme.fg(color, glyph)} ${theme.fg("muted", "subagent")} ${countStatuses(details.children.map((child) => child.status))}`;
    }
    case "subagent-overrun": {
      const shell = details.shell
        ? ` · shell ${formatDuration(details.shell.elapsedMs)}${details.shell.growing === null ? "" : details.shell.growing ? " (output growing)" : " (output idle)"}`
        : "";
      return `${theme.fg("warning", "!")} ${theme.fg("muted", "overrun")} ${details.name} ${formatDuration(details.elapsedMs)} / ${formatDuration(details.budgetMs)} budget · still running${shell}`;
    }
    case "supervisor-request":
      return [
        `${theme.fg("warning", "?")} ${theme.fg("muted", `decision for ${details.name}`)} ${details.message.slice(0, 100)}`,
        theme.fg("dim", `/reply ${details.from} <decision>`),
      ].join("\n");
    case "supervisor-update":
      return `${theme.fg("muted", "↑")} ${theme.fg("muted", "supervisor update")} ${details.message.slice(0, 100)}`;
  }
}

export function expandedWakeText(details: FamulusWake | undefined, content: string): string {
  if (!details) {
    return content
      .replace(FAMULUS_WAKE_LEAD_IN, "")
      .replace(/<[^>]+>/g, " ")
      .replace(/&lt;/g, "<")
      .replace(/&gt;/g, ">")
      .replace(/&amp;/g, "&")
      .trim();
  }
  switch (details.kind) {
    case "task":
      return [
        `Tasks (${details.tasks.length})`,
        ...details.tasks.map((task) => [
          `${task.id} · ${task.taskKind} · ${task.status} · ${formatDuration(task.durationMs)} · ${taskExit(task)}`,
          `$ ${task.command}`,
          `Output: ${task.outputPath}`,
          ...(task.preview ? [`Preview: ${task.preview}`] : []),
        ].join("\n")),
        ...(details.stillRunning.length ? [`Still running (${details.stillRunning.length}): ${details.stillRunning.map((item) => `${item.id} ${item.title}`).join(", ")}`] : []),
      ].join("\n\n");
    case "monitor":
      return [`Monitor: ${details.description}`, `Task: ${details.id}`, ...(details.status ? [`Status: ${details.status}`] : []), `Event: ${details.event}`, ...(details.eventCount ? [`Events: ${details.eventCount}`] : []), ...(details.droppedLines ? [`Dropped lines: ${details.droppedLines}`] : [])].join("\n");
    case "subagent-handover":
      return [`Subagent handover: ${details.name} (${details.status})`, `Run: ${details.runId}`, `Child: ${details.childId}`, `Task prompt: ${details.prompt}`, `Result: ${details.result}`, ...(details.error ? [`Error: ${details.error}`] : []), ...(details.warning ? [`Warning: ${details.warning}`] : []), ...(details.stillRunning.length ? [`Still running: ${details.stillRunning.map((item) => `${item.id} ${item.title}`).join(", ")}`] : [])].join("\n\n");
    case "subagent-done":
      return [`Subagent run: ${details.runId} (${details.status})`, `Duration: ${formatDuration(details.durationMs)}`, ...details.children.map((child) => [`${child.name} (${child.childId}) · ${child.status}`, `Task prompt: ${child.prompt}`, `Result: ${child.result}`, ...(child.error ? [`Error: ${child.error}`] : []), ...(child.warning ? [`Warning: ${child.warning}`] : [])].join("\n"))].join("\n\n");
    case "subagent-overrun": {
      const shell = details.shell;
      return [
        `Subagent past its budget: ${details.name} (still running, reminder ${details.reminder})`,
        `Run: ${details.runId}`,
        `Child: ${details.childId}`,
        `Elapsed this turn: ${formatDuration(details.elapsedMs)} of ${formatDuration(details.budgetMs)}`,
        `Last activity (${formatDuration(details.lastActivity.agoMs)} ago): ${details.lastActivity.text}`,
        ...(shell
          ? [
              [
                `Shell ${shell.taskId} · ${formatDuration(shell.elapsedMs)}`,
                `$ ${shell.command}`,
                `Output: ${shell.outputPath}${shell.outputBytes !== null ? ` (${shell.outputBytes} bytes)` : ""}`,
                ...(shell.outputIdleMs !== null ? [`Last output: ${formatDuration(shell.outputIdleMs)} ago`] : []),
              ].join("\n"),
            ]
          : []),
        ...(details.hardCeilingMs !== undefined
          ? [`Hard ceiling stops it in ${formatDuration(details.hardCeilingMs)} (extend does not move it).`]
          : []),
        // Same condition as overrunOptions in wake.ts: do not promise a
        // reminder that the hard ceiling will stop the child before.
        ...(details.hardCeilingMs === undefined || details.nextReminderMs < details.hardCeilingMs
          ? [`Next reminder in ${formatDuration(details.nextReminderMs)} unless you extend, steer, or interrupt it.`]
          : []),
      ].join("\n\n");
    }
    case "supervisor-request":
      return [`Decision requested by ${details.name} (${details.from})`, `Request: ${details.message}`, `Reply with /reply ${details.from} <decision>`].join("\n");
    case "supervisor-update":
      return [`Update from ${details.name} (${details.from})`, details.message].join("\n");
  }
}

function wakeDetails(message: { details?: unknown }): FamulusWake | undefined {
  const details = message.details as FamulusWake | undefined;
  if (!details || typeof details !== "object" || !("kind" in details)) return undefined;
  return details;
}

function makeComponent(pad: number, theme: Theme, text: string): PillComponent {
  const tui = loadPiTui();
  if (tui) {
    // paddingX = outputPad, paddingY = 1 (blank line above/below), then bg.
    const box = new tui.Box(pad, 1, (s) => theme.bg("customMessageBg", s));
    box.addChild(new tui.Text(text, 0, 0));
    const rendered = box as unknown as { render(width: number): string[]; invalidate(): void };
    if (typeof rendered.invalidate !== "function") {
      rendered.invalidate = () => {};
    }
    return rendered;
  }
  return {
    render(width: number) {
      const inner = Math.max(1, width - pad * 2);
      const padStr = " ".repeat(Math.max(0, pad));
      return fitLines(text, inner).map((line) => padStr + line);
    },
    invalidate() {},
  };
}

export function registerFamulusMessageRenderers(pi: ExtensionAPI): void {
  pi.registerMessageRenderer(FAMULUS_WAKE_CUSTOM_TYPE, (message, { expanded, outputPad }, theme) => {
    const details = wakeDetails(message);
    const content = typeof message.content === "string" ? message.content : "";
    const head = details ? collapsedText(details, theme) : theme.fg("muted", "wake");
    const body = expanded && content ? `\n${theme.fg("dim", expandedWakeText(details, content))}` : "";
    return makeComponent(outputPad, theme, head + body) as never;
  });
}

