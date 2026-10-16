/**
 * Subagent conversation as a thread (same visual language as `pi-famulus top`).
 *
 *   ┃ main → scout                           message from the parent
 *   ┃ inspect the loader and report          on an accent rail
 *
 *   ● scout                                  the child starts speaking
 *     ▸ ∴ Thinking                           folded reasoning
 *     ▸ ✓ read  src/loader.ts                one line per tool call
 *     ▾ ✓ bash  rg loader                    unfolded: output hangs off a rule
 *     │ src/loader.ts:12
 *
 *     The loader skips `p5`.                 assistant prose, light markdown
 *
 * Consecutive tool / thinking rows stack tight; any other change of block
 * gets one blank line. Colors come from the pi theme, so light and dark
 * themes both work.
 */
import type { ConversationTurn } from "../subagent/types";
import { fitLines, truncateToWidth, visibleWidth } from "./pi-tui-load";

export interface ChatTheme {
  fg(color: string, text: string): string;
  bold(text: string): string;
  italic(text: string): string;
}

export interface ChatOptions {
  /** Unfold every tool output and thinking block. */
  expanded: boolean;
  /** Child display name for the speaker headers (default "subagent"). */
  agent?: string;
}

const MARGIN = "  ";

type Group = "user" | "text" | "activity";

interface ToolCall {
  name: string;
  summary: string;
}

/** `tool read {"path":"src"}` → name + one-line summary of the arguments. */
export function parseToolCall(text: string): ToolCall | undefined {
  const match = /^tool (\S+?)(?:\s*(\{[\s\S]*))?$/.exec(text.trim());
  if (!match) return undefined;
  return { name: match[1], summary: argsSummary(match[2] ?? "") };
}

function argsSummary(raw: string): string {
  if (!raw) return "";
  let args: Record<string, unknown> | undefined;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed && typeof parsed === "object") args = parsed as Record<string, unknown>;
  } catch {
    // Truncated or not JSON: fall back to the well-known keys below.
  }
  for (const key of ["path", "command", "query", "pattern", "file", "description", "name"]) {
    const value = args ? args[key] : new RegExp(`"${key}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)`).exec(raw)?.[1];
    if (typeof value === "string" && value.trim()) return firstLine(value);
  }
  return "";
}

function firstLine(text: string): string {
  return text.trim().split("\n")[0] ?? "";
}

/** `tool read` / `tool read error` → `{ name, error }`. */
function toolResult(turn: ConversationTurn): { name: string; error: boolean } | undefined {
  const match = /^tool (\S+)( error)?$/.exec(turn.role);
  return match ? { name: match[1], error: Boolean(match[2]) } : undefined;
}

/** Strip CSI/OSC and collapse carriage-return progress, as a terminal would paint it. */
export function scrubTty(text: string): string {
  return text
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[@-Z\\-_]/g, "")
    .split("\n")
    .map((line) => {
      const parts = line.split("\r").filter((part) => part.length > 0);
      return (parts.at(-1) ?? "").replace(/\t/g, "    ");
    })
    .join("\n");
}

function padBetween(left: string, right: string, width: number): string {
  if (!right) return truncateToWidth(left, width, "…");
  const rw = visibleWidth(right);
  const room = width - rw - 2;
  if (room < 8) return truncateToWidth(left, width, "…");
  const l = truncateToWidth(left, room, "…");
  return l + " ".repeat(Math.max(2, width - visibleWidth(l) - rw)) + right;
}

/** Inline `code` and **bold**. State does not carry across source lines. */
function inline(line: string, theme: ChatTheme): string {
  return line
    .replace(/`([^`]+)`/g, (_m, code: string) => theme.fg("mdCode", code))
    .replace(/\*\*([^*]+)\*\*/g, (_m, strong: string) => theme.bold(strong));
}

function hang(text: string, color: string, width: number, theme: ChatTheme, italic = false): string[] {
  const rule = theme.fg("borderMuted", "│ ");
  const inner = Math.max(1, width - MARGIN.length - 2);
  const out: string[] = [];
  for (const raw of text.split("\n")) {
    for (const line of fitLines(raw, inner)) {
      const body = theme.fg(color, line);
      out.push(MARGIN + rule + (italic ? theme.italic(body) : body));
    }
  }
  return out;
}

function markdown(text: string, width: number, theme: ChatTheme): string[] {
  const out: string[] = [];
  const inner = Math.max(1, width - MARGIN.length);
  let fence = false;
  for (const raw of text.split("\n")) {
    const trimmed = raw.trimStart();
    if (trimmed.startsWith("```")) {
      fence = !fence;
      continue;
    }
    if (fence) {
      for (const line of fitLines(scrubTty(raw), Math.max(1, inner - 2))) {
        out.push(MARGIN + theme.fg("mdCodeBlockBorder", "│ ") + theme.fg("mdCodeBlock", line));
      }
      continue;
    }
    if (!raw.trim()) {
      out.push("");
      continue;
    }
    const heading = /^#{1,6} (.*)$/.exec(trimmed);
    if (heading) {
      for (const line of fitLines(heading[1], inner)) out.push(MARGIN + theme.bold(theme.fg("mdHeading", line)));
      continue;
    }
    const indent = " ".repeat(Math.min(8, raw.length - trimmed.length));
    const bullet = /^[-*] (.*)$/.exec(trimmed);
    const ordered = /^(\d{1,3}[.)] )(.*)$/.exec(trimmed);
    const quote = /^> ?(.*)$/.exec(trimmed);
    let lead = indent;
    let body = trimmed;
    let leadStyled = indent;
    if (bullet) {
      lead = `${indent}• `;
      leadStyled = indent + theme.fg("mdListBullet", "• ");
      body = bullet[1];
    } else if (ordered) {
      lead = indent + ordered[1];
      leadStyled = indent + theme.fg("mdListBullet", ordered[1]);
      body = ordered[2];
    } else if (quote) {
      lead = "│ ";
      leadStyled = theme.fg("mdQuoteBorder", "│ ");
      body = quote[1];
    }
    const styled = quote ? theme.fg("mdQuote", inline(body, theme)) : inline(body, theme);
    const lines = fitLines(styled, Math.max(1, inner - lead.length));
    lines.forEach((line, i) => {
      out.push(MARGIN + (i === 0 || quote ? leadStyled : " ".repeat(lead.length)) + line);
    });
  }
  return out;
}

export function renderConversation(
  turns: readonly ConversationTurn[],
  width: number,
  theme: ChatTheme,
  options: ChatOptions,
): string[] {
  const w = Math.max(1, width);
  if (turns.length === 0) return [MARGIN + theme.fg("dim", "(no conversation yet)")];
  const out: string[] = [];
  let last: Group | undefined;
  const agent = options.agent?.trim() || "subagent";
  // Blank line between blocks, and a `● agent` header whenever the child
  // starts speaking after the parent (or at the top of the thread).
  const begin = (group: Group) => {
    if (last !== undefined && !(group === "activity" && last === "activity")) out.push("");
    if (group !== "user" && (last === undefined || last === "user")) {
      out.push(theme.fg("success", "● ") + theme.bold(theme.fg("success", agent)));
    }
    last = group;
  };
  const chevron = (open: boolean, foldable: boolean) =>
    theme.fg("dim", foldable ? (open ? "▾ " : "▸ ") : "  ");

  const toolRow = (call: ToolCall, result: { text: string; error: boolean } | undefined) => {
    begin("activity");
    const pending = result === undefined;
    const body = pending ? "" : scrubTty(result.text).trimEnd();
    const foldable = body.trim().length > 0;
    const glyph = pending
      ? theme.fg("accent", "● ")
      : result.error ? theme.fg("error", "✗ ") : theme.fg("success", "✓ ");
    const summary = call.summary ? theme.fg("muted", `  ${call.summary}`) : "";
    const tail = pending ? theme.fg("dim", "running") : "";
    out.push(padBetween(MARGIN + chevron(options.expanded, foldable) + glyph + theme.bold(call.name) + summary, tail, w));
    if (foldable && options.expanded) out.push(...hang(body, result?.error ? "error" : "toolOutput", w, theme));
  };

  for (let i = 0; i < turns.length; i++) {
    const turn = turns[i];
    if (turn.role === "user") {
      begin("user");
      const rail = theme.fg("accent", "┃ ");
      out.push(rail + theme.bold(theme.fg("accent", "main")) + theme.fg("dim", ` → ${agent}`));
      for (const line of fitLines(turn.text.trim() || " ", w - 2)) out.push(rail + line);
      continue;
    }
    if (turn.role === "thinking") {
      begin("activity");
      const foldable = turn.text.trim().length > 0;
      out.push(MARGIN + chevron(options.expanded, foldable) + theme.fg("muted", "∴ ") + theme.italic(theme.fg("dim", "Thinking")));
      if (foldable && options.expanded) out.push(...hang(scrubTty(turn.text.trim()), "thinkingText", w, theme, true));
      continue;
    }
    const result = toolResult(turn);
    if (result) {
      // A result without its call (the call scrolled out of the snapshot).
      toolRow({ name: result.name, summary: "" }, { text: turn.text, error: result.error });
      continue;
    }
    if (turn.role === "assistant error") {
      begin("text");
      fitLines(turn.text.trim(), w - MARGIN.length - 2).forEach((line, j) => {
        out.push(MARGIN + (j === 0 ? theme.fg("error", "✗ ") : "  ") + theme.fg("error", line));
      });
      continue;
    }
    const call = parseToolCall(turn.text);
    if (call) {
      const next = turns[i + 1];
      const paired = next ? toolResult(next) : undefined;
      if (next && paired && paired.name === call.name) {
        toolRow(call, { text: next.text, error: paired.error });
        i++;
      } else {
        toolRow(call, undefined);
      }
      continue;
    }
    begin("text");
    out.push(...markdown(turn.text.trim(), w, theme));
  }
  return out.map((line) => (visibleWidth(line) > w ? truncateToWidth(line, w, "…") : line));
}

/** Plain-text thread for notify fallbacks and logs: same shape, no color. */
export const plainTheme: ChatTheme = {
  fg: (_color, text) => text,
  bold: (text) => text,
  italic: (text) => text,
};
