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
  /** Raw JSON arguments, possibly cut short; read leniently. */
  args?: string;
}

/** `tool read {"path":"src"}` → name + one-line summary of the arguments. */
export function parseToolCall(text: string): ToolCall | undefined {
  const match = /^tool (\S+?)(?:\s*(\{[\s\S]*))?$/.exec(text.trim());
  if (!match) return undefined;
  const args = stripCapSuffix(match[2] ?? "");
  return { name: match[1], summary: argsSummary(args), args };
}

/** Drop a transcript cap marker such as `… (3666 chars)`. */
function stripCapSuffix(raw: string): string {
  return raw.replace(/… \(\d+ chars\)\s*$/, "");
}

/** Every string value of `key`, in order, from JSON that may be cut short. */
export function jsonStrings(json: string, key: string): Array<{ value: string; closed: boolean }> {
  const out: Array<{ value: string; closed: boolean }> = [];
  const pat = new RegExp(`"${key}"\\s*:\\s*"`, "g");
  let m: RegExpExecArray | null;
  while ((m = pat.exec(json)) !== null) {
    let value = "";
    let closed = false;
    let i = m.index + m[0].length;
    for (; i < json.length; i++) {
      const c = json[i];
      if (c === '"') {
        closed = true;
        break;
      }
      if (c !== "\\") {
        value += c;
        continue;
      }
      const e = json[++i];
      if (e === undefined) break;
      if (e === "n") value += "\n";
      else if (e === "t") value += "\t";
      else if (e === "r") continue;
      else if (e === "u") {
        const code = Number.parseInt(json.slice(i + 1, i + 5), 16);
        if (!Number.isNaN(code)) value += String.fromCharCode(code);
        i += 4;
      } else value += e;
    }
    out.push({ value, closed });
    pat.lastIndex = Math.max(pat.lastIndex, i);
  }
  return out;
}

/** What a write / edit call changes: a stat for its row, rows for unfolding. */
export interface CallPreview {
  stat: string;
  rows: Array<{ gutter: string; color: string; text: string }>;
  cut: boolean;
}

export function callPreview(name: string, args: string | undefined, theme: ChatTheme): CallPreview | undefined {
  if (!args) return undefined;
  if (name === "write") {
    const content = jsonStrings(args, "content")[0];
    if (!content) return undefined;
    const text = content.value.replace(/\n$/, "");
    const lines = content.value === "" ? [] : text.split("\n");
    const digits = String(lines.length).length;
    const count = content.closed ? `+${lines.length} line${lines.length === 1 ? "" : "s"}` : `+${lines.length}+ lines`;
    return {
      stat: theme.fg("success", count),
      rows: lines.map((text, i) => ({ gutter: `${String(i + 1).padStart(digits)} `, color: "text", text })),
      cut: !content.closed,
    };
  }
  if (name === "edit") {
    const olds = jsonStrings(args, "oldText");
    const news = jsonStrings(args, "newText");
    const pairs = Math.max(olds.length, news.length);
    if (pairs === 0) return undefined;
    const rows: CallPreview["rows"] = [];
    let minus = 0;
    let plus = 0;
    let cut = false;
    for (let k = 0; k < pairs; k++) {
      if (pairs > 1) rows.push({ gutter: "", color: "dim", text: `@@ edit ${k + 1}/${pairs}` });
      const old = olds[k];
      const neu = news[k];
      if (old) {
        cut ||= !old.closed;
        for (const line of old.value.split("\n")) {
          minus++;
          rows.push({ gutter: "- ", color: "toolDiffRemoved", text: line });
        }
      }
      if (neu) {
        cut ||= !neu.closed;
        for (const line of neu.value.split("\n")) {
          plus++;
          rows.push({ gutter: "+ ", color: "toolDiffAdded", text: line });
        }
      }
    }
    const edits = pairs > 1 ? theme.fg("dim", ` · ${pairs} edits`) : "";
    return { stat: `${theme.fg("error", `−${minus}`)} ${theme.fg("success", `+${plus}`)}${edits}`, rows, cut };
  }
  return undefined;
}

const PREVIEW_ROWS = 60;

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
    const preview = callPreview(call.name, call.args, theme);
    const foldable = preview !== undefined || body.trim().length > 0;
    const glyph = pending
      ? theme.fg("accent", "● ")
      : result.error ? theme.fg("error", "✗ ") : theme.fg("success", "✓ ");
    const summary = call.summary ? theme.fg("muted", `  ${scrubTty(call.summary)}`) : "";
    // The stat sits on the right so a long path truncates, not the stat.
    const tail = [preview?.stat, pending ? theme.fg("dim", "running") : ""].filter(Boolean).join("  ");
    out.push(padBetween(MARGIN + chevron(options.expanded, foldable) + glyph + theme.bold(call.name) + summary, tail, w));
    if (!foldable || !options.expanded) return;
    if (preview) {
      const rule = theme.fg("borderMuted", "│ ");
      const gw = Math.max(0, ...preview.rows.map((r) => r.gutter.length));
      const inner = Math.max(1, w - MARGIN.length - 2 - gw);
      let shown = 0;
      let rows = 0;
      for (const row of preview.rows) {
        const gutterColor = /^\s*\d+ $/.test(row.gutter) ? "dim" : row.color;
        const segs = fitLines(scrubTty(row.text), inner);
        const room = PREVIEW_ROWS - rows;
        segs.slice(0, room).forEach((seg, j) => {
          // Headers (no gutter) sit flush; gutters align under each other.
          const g = row.gutter === "" ? "" : j === 0 ? row.gutter.padStart(gw) : " ".repeat(gw);
          out.push(MARGIN + rule + theme.fg(gutterColor, g) + theme.fg(row.color, seg));
        });
        rows += Math.min(room, segs.length);
        // A line the cap cut part-way through counts as not shown.
        if (segs.length > room) break;
        shown++;
      }
      const more = preview.rows.length - shown;
      if (more > 0) out.push(MARGIN + rule + theme.fg("dim", `… ${more} more line${more === 1 ? "" : "s"}`));
      if (preview.cut) out.push(MARGIN + rule + theme.fg("dim", "… cut short in the transcript"));
      if (body.trim()) out.push(...hang(body, result?.error ? "error" : "dim", w, theme));
      return;
    }
    out.push(...hang(body, result?.error ? "error" : "toolOutput", w, theme));
  };

  for (let i = 0; i < turns.length; i++) {
    const turn = turns[i];
    if (turn.role === "user") {
      begin("user");
      const rail = theme.fg("accent", "┃ ");
      out.push(rail + theme.bold(theme.fg("accent", "main")) + theme.fg("dim", ` → ${agent}`));
      for (const line of fitLines(scrubTty(turn.text).trim() || " ", w - 2)) out.push(rail + line);
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
      fitLines(scrubTty(turn.text).trim(), w - MARGIN.length - 2).forEach((line, j) => {
        out.push(MARGIN + (j === 0 ? theme.fg("error", "✗ ") : "  ") + theme.fg("error", line));
      });
      continue;
    }
    const call = parseToolCall(turn.text);
    if (call) {
      // Parallel calls arrive as a run of calls, then a run of results in
      // call order: pair them by name, first unused result first.
      const calls = [call];
      let j = i + 1;
      for (let c: ToolCall | undefined; j < turns.length && !toolResult(turns[j]) && (c = parseToolCall(turns[j].text)); j++) calls.push(c);
      const results: Array<{ name: string; error: boolean; text: string; used: boolean }> = [];
      for (let r; j < turns.length && (r = toolResult(turns[j])); j++) results.push({ ...r, text: turns[j].text, used: false });
      for (const c of calls) {
        const hit = results.find((r) => !r.used && r.name === c.name);
        if (hit) hit.used = true;
        toolRow(c, hit && { text: hit.text, error: hit.error });
      }
      // A result with no matching call (its call scrolled out of the snapshot).
      for (const r of results) if (!r.used) toolRow({ name: r.name, summary: "" }, r);
      i = j - 1;
      continue;
    }
    begin("text");
    // Child transcript text is untrusted: no escape sequence reaches the terminal.
    out.push(...markdown(scrubTty(turn.text).trim(), w, theme));
  }
  return out.map((line) => (visibleWidth(line) > w ? truncateToWidth(line, w, "…") : line));
}

/** Plain-text thread for notify fallbacks and logs: same shape, no color. */
export const plainTheme: ChatTheme = {
  fg: (_color, text) => text,
  bold: (text) => text,
  italic: (text) => text,
};
