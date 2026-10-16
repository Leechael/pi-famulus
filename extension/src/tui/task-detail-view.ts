/**
 * /tasks detail for a shell or monitor, matching `pi-famulus top`:
 *
 *   ◆ sh_ab12 · pytest -n 4                                    ● following
 *   ● running   shell   12s   exit -                         pinned info
 *   $ pytest -n 4
 *   cwd ~/app   started 14:45:32   run run_1   …/sh_ab12.output
 *   ─┐1 output┌┐2 stderr┌──────────────────────── 24 lines · 12 blank hidden ─
 *   …output tail, blank lines hidden, matches highlighted…
 *   / search  & filter  b show blank  1 2 tab  f follow  esc close
 *
 * `/` searches as you type: matches highlight and `n` / `N` step through
 * them, or `&` (Tab while typing) keeps only matching lines. Lowercase is
 * case-insensitive, an uppercase letter makes it case-sensitive, `a|b`
 * matches either.
 */
import type { ExtensionUIContext } from "@earendil-works/pi-coding-agent";
import { realClock, type Clock, type ClockTimer } from "../clock";
import { formatAge, type WorkItem } from "../work-index";
import { scrubTty } from "./conversation-view";
import { fitLines, loadPiTui, truncateToWidth, visibleWidth } from "./pi-tui-load";
import { fitKeys, positionPill, spread, tabRow, withThumb, type Key, type PaneTheme } from "./scroll-detail-view";
import { readTaskFileTailCached } from "./task-output-paths";
import { statusGlyph } from "./tool-component";

export type TaskStream = "output" | "stderr";

export interface TaskTheme extends PaneTheme {
  bg?(color: string, text: string): string;
  inverse?(text: string): string;
}

export interface LogView {
  query: string;
  editing: boolean;
  filter: boolean;
  showBlank: boolean;
}

export interface Query {
  terms: string[];
  fold: boolean;
}

export function parseQuery(query: string): Query | null {
  const fold = !/[A-Z]/.test(query);
  const terms = query
    .split("|")
    .map((term) => term.trim())
    .filter((term) => term.length > 0)
    .map((term) => (fold ? term.toLowerCase() : term));
  return terms.length > 0 ? { terms, fold } : null;
}

/** Merged [start, end) ranges of every match, left to right. */
export function matchRanges(line: string, query: Query): Array<[number, number]> {
  const hay = query.fold ? line.toLowerCase() : line;
  const found: Array<[number, number]> = [];
  for (const term of query.terms) {
    let from = 0;
    for (;;) {
      const at = hay.indexOf(term, from);
      if (at < 0) break;
      found.push([at, at + term.length]);
      from = at + term.length;
    }
  }
  found.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const merged: Array<[number, number]> = [];
  for (const [start, end] of found) {
    const last = merged.at(-1);
    if (last && start <= last[1]) last[1] = Math.max(last[1], end);
    else merged.push([start, end]);
  }
  return merged;
}

function hit(theme: TaskTheme, text: string, current: boolean): string {
  if (current) return theme.inverse ? theme.inverse(theme.bold(text)) : theme.bold(theme.fg("warning", text));
  try {
    if (theme.bg) return theme.bg("searchMatchBg", theme.fg("searchMatchText", text));
  } catch {
    // Themes without the optional search colors fall through.
  }
  return theme.bold(theme.fg("warning", text));
}

function highlight(segment: string, query: Query | null, theme: TaskTheme, current: boolean): { text: string; hit: boolean } {
  const ranges = query ? matchRanges(segment, query) : [];
  if (ranges.length === 0) return { text: segment, hit: false };
  let out = "";
  let at = 0;
  for (const [start, end] of ranges) {
    out += segment.slice(at, start) + hit(theme, segment.slice(start, end), current);
    at = end;
  }
  return { text: out + segment.slice(at), hit: true };
}

export interface LogLayout {
  lines: string[];
  /** Body rows holding a match. */
  matches: number[];
  /** Source lines shown / in the tail. */
  shown: number;
  total: number;
}

/** Placeholder bodies such as "(no output yet)". */
function isPlaceholder(text: string): boolean {
  return /^\([^\n]*\)$/.test(text.trim());
}

export function layoutLog(text: string, width: number, view: LogView, theme: TaskTheme, currentRow = -1): LogLayout {
  const inner = Math.max(1, width - 1);
  const source = text.replace(/\n$/, "").split("\n");
  const out: LogLayout = { lines: [], matches: [], shown: 0, total: source.length };
  if (isPlaceholder(text)) {
    return { lines: [truncateToWidth(` ${theme.fg("dim", text.trim())}`, Math.max(1, width), "…")], matches: [], shown: 1, total: 1 };
  }
  const query = parseQuery(view.query);
  for (const raw of source) {
    const clean = scrubTty(raw.replace(/\r+$/, ""));
    if (!view.showBlank && clean.trim() === "") continue;
    if (view.filter && query && matchRanges(clean, query).length === 0) continue;
    out.shown++;
    for (const segment of fitLines(clean, inner)) {
      const row = out.lines.length;
      const painted = highlight(segment, query, theme, row === currentRow);
      if (painted.hit) out.matches.push(row);
      out.lines.push(` ${painted.text}`);
    }
  }
  if (out.lines.length === 0) {
    out.lines.push(` ${theme.fg("dim", query && view.filter ? "(no line matches)" : "(only blank lines)")}`);
  }
  return out;
}

function clockOf(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function tilde(path: string): string {
  const home = process.env.HOME;
  return home && path.startsWith(home) ? `~${path.slice(home.length)}` : path;
}

/** Pinned panel: state and outcome, the command, and where it runs. */
export function infoCard(item: WorkItem, now: number, theme: TaskTheme, width: number): string[] {
  const { color, glyph } = statusGlyph(item.status);
  const live = item.status === "running" || item.status === "pending";
  const pair = (label: string, value: string) => `${theme.fg("dim", `   ${label} `)}${value}`;
  let line1 = `${theme.fg(color, glyph)} ${theme.bold(theme.fg(live ? "success" : color, item.status))}${theme.fg("muted", `   ${item.kind}`)}`;
  line1 += pair("wall", formatAge(item.startedAt, item.endedAt, now));
  if (item.exitCode !== undefined && item.exitCode !== null) {
    line1 += pair("exit", theme.bold(theme.fg(item.exitCode === 0 ? "success" : "error", String(item.exitCode))));
  } else if (item.signal) {
    line1 += pair("signal", theme.fg("error", item.signal));
  }
  if (item.endReason && item.endReason !== "exited") line1 += pair("ended", item.endReason);
  if (item.error) line1 += pair("error", theme.fg("error", item.error));
  const command = (item.command ?? item.title).trimEnd();
  const first = command.split("\n")[0] ?? "";
  const line2 = `${theme.bold(theme.fg("accent", "$ "))}${theme.bold(first)}${command.includes("\n") ? theme.fg("dim", " …") : ""}`;
  const parts: string[] = [];
  if (item.cwd) parts.push(`${theme.fg("dim", "cwd ")}${theme.fg("muted", tilde(item.cwd))}`);
  parts.push(`${theme.fg("dim", "started ")}${theme.fg("muted", clockOf(item.startedAt))}`);
  if (item.runId) parts.push(`${theme.fg("dim", "run ")}${theme.fg("muted", item.runId)}`);
  let line3 = parts.join("   ");
  if (item.outputPath) {
    const room = width - visibleWidth(line3) - 3;
    if (room > 12) line3 += `   ${theme.fg("dim", truncateLeft(item.outputPath, room))}`;
  }
  return [line1, line2, line3].map((line) => truncateToWidth(line, Math.max(1, width), "…"));
}

function truncateLeft(text: string, width: number): string {
  return text.length <= width ? text : `…${text.slice(text.length - width + 1)}`;
}

/** File tail, or why it is empty (live vs finished). */
export function streamBody(item: WorkItem, path: string, stream: TaskStream): string {
  const text = readTaskFileTailCached(path);
  if (text !== "(empty)") return text;
  const live = item.status === "running" || item.status === "pending";
  if (stream === "stderr") return live ? "(nothing on stderr yet)" : "(nothing on stderr)";
  return live ? "(no output yet)" : "(no output; the command printed nothing)";
}

export interface TaskDetailOptions {
  title: string;
  /** Read on every render so the panel follows the task's status. */
  getItem: () => WorkItem;
  paths: { outputPath: string; stderrPath: string };
  pollMs?: number;
  clock?: Clock;
}

const INFO_ROWS = 3;

export async function showTaskDetail(ui: ExtensionUIContext, options: TaskDetailOptions): Promise<void> {
  const piTui = loadPiTui();
  if (!piTui) throw new Error("pi-tui is not available");
  const { matchesKey } = piTui;
  const tabs: TaskStream[] = ["output", "stderr"];
  const clock = options.clock ?? realClock;
  await ui.custom<void>(
    (tui, rawTheme, _kb, done) => {
      const theme = rawTheme as unknown as TaskTheme;
      let stream: TaskStream = "output";
      const view: LogView = { query: "", editing: false, filter: false, showBlank: false };
      let follow = true;
      let scroll = 0;
      let current = 0;
      let matches: number[] = [];
      let seek = false;
      let tabHits: Array<[number, number, TaskStream]> = [];
      let tabY = INFO_ROWS + 1;
      let lastHeight = 1;
      let disposed = false;
      let timer: ClockTimer | null = null;
      if (options.pollMs && options.pollMs > 0) {
        timer = clock.setInterval(() => {
          if (!disposed) tui.requestRender();
        }, options.pollMs);
        clock.unref?.(timer);
      }
      const bodyHeight = () => Math.max(1, Math.max(10, tui.terminal?.rows ?? 24) - (1 + INFO_ROWS + 1 + 1));
      const setStream = (next: TaskStream) => {
        if (next === stream) return;
        stream = next;
        scroll = 0;
        follow = true;
        current = 0;
        matches = [];
      };
      const jump = (forward: boolean) => {
        if (matches.length === 0) return;
        current = forward ? (current + 1) % matches.length : (current + matches.length - 1) % matches.length;
        follow = false;
        scroll = Math.max(0, matches[current] - Math.floor(lastHeight / 3));
      };
      const render = (width: number): string[] => {
        const item = options.getItem();
        const now = clock.now();
        const height = bodyHeight();
        lastHeight = height;
        const path = stream === "stderr" ? options.paths.stderrPath : options.paths.outputPath;
        const laid = layoutLog(streamBody(item, path, stream), width - 1, view, theme, matches[current] ?? -1);
        matches = laid.matches;
        const max = Math.max(0, laid.lines.length - height);
        if (seek && matches.length > 0) {
          const next = matches.findIndex((row) => row >= scroll);
          current = next >= 0 ? next : matches.length - 1;
          follow = false;
          scroll = Math.max(0, matches[current] - Math.floor(height / 3));
        }
        seek = false;
        current = Math.min(current, Math.max(0, matches.length - 1));
        if (follow || scroll >= max) {
          scroll = max;
          follow = true;
        }
        const dim = (s: string) => theme.fg("dim", s);
        const live = item.status === "running" || item.status === "pending";
        const pill = positionPill(theme, follow, live, laid.lines.length, height, scroll);
        const head = spread(`${theme.fg("accent", "◆")} ${theme.bold(options.title)}`, pill, width);
        const q = view.query.length > 0;
        let right: string;
        const lines = (n: number) => (n === 1 ? " line" : " lines");
        if (q && view.filter) right = `${laid.shown}${dim(` of ${laid.total}${lines(laid.total)}`)}`;
        else if (q) {
          const at = matches.length === 0 ? 0 : current + 1;
          right = `${theme.fg(matches.length === 0 ? "error" : "text", `${at}/${matches.length}`)}${dim(" matching rows")}`;
        } else {
          const hidden = laid.total - laid.shown;
          right = `${laid.total}${dim(lines(laid.total))}${hidden > 0 && !view.showBlank ? dim(` · ${hidden} blank hidden`) : ""}`;
        }
        const row = tabRow(tabs, stream, right, theme, width);
        tabHits = row.hits;
        tabY = 1 + INFO_ROWS;
        const visible = laid.lines.slice(scroll, scroll + height);
        while (visible.length < height) visible.push("");
        const body = withThumb(visible, width, laid.lines.length, height, scroll, theme);
        // Keys name the mode they switch to; the least important drop first.
        const mode = view.filter ? "highlight" : "filter";
        let foot: string;
        if (view.editing) {
          const prompt = `${theme.bold(theme.fg("accent", "/ "))}${view.query}${theme.fg("accent", "▏")}`;
          foot = fitKeys(theme, prompt, [["enter", "apply", 0], ["tab", mode, 1], ["esc", "clear", 0]], width - 1);
        } else {
          const keys: Key[] = q ? [["n N", "next", 1]] : [["/", "search", 1]];
          keys.push(["&", mode, 2], ["b", view.showBlank ? "hide blank" : "show blank", 4], ["1 2", "stream", 5]);
          if (live) keys.push(["f", follow ? "unfollow" : "follow", 3]);
          keys.push(["esc", q ? "clear" : "close", 0]);
          foot = fitKeys(theme, q ? `${theme.fg("accent", "/ ")}${view.query}` : "", keys, width - 1);
        }
        return [head, ...infoCard(item, now, theme, width), row.line, ...body, ` ${foot}`];
      };
      const searchInput = (data: string) => {
        if (matchesKey(data, "escape")) {
          view.query = "";
          view.editing = false;
        } else if (matchesKey(data, "enter") || data === "\r" || data === "\n") view.editing = false;
        else if (matchesKey(data, "tab") || data === "\t") view.filter = !view.filter;
        else if (data === "\u007f" || data === "\b") {
          view.query = view.query.slice(0, -1);
          seek = true;
        } else if (!data.startsWith("\u001b") && [...data].every((ch) => ch >= " ")) {
          view.query += data;
          seek = true;
        }
        tui.requestRender();
      };
      return {
        render,
        invalidate() {},
        handleInput(data: string) {
          if (view.editing) {
            searchInput(data);
            return;
          }
          const move = (next: number) => {
            scroll = Math.max(0, next);
            follow = false;
            tui.requestRender();
          };
          if (matchesKey(data, "escape")) {
            if (view.query) {
              view.query = "";
              matches = [];
              tui.requestRender();
              return;
            }
            done();
            return;
          }
          if (data === "q") return done();
          if (data === "1") setStream("output");
          else if (data === "2") setStream("stderr");
          else if (matchesKey(data, "tab") || data === "\t") setStream(stream === "output" ? "stderr" : "output");
          else if (data === "/") {
            view.editing = true;
            seek = true;
          } else if (data === "&") view.filter = !view.filter;
          else if (data === "n") jump(true);
          else if (data === "N") jump(false);
          else if (data === "b") view.showBlank = !view.showBlank;
          else if (data === "f" || data === "F") follow = !follow;
          else if (matchesKey(data, "up") || data === "k") return move(scroll - 1);
          else if (matchesKey(data, "down") || data === "j") return move(scroll + 1);
          else if (matchesKey(data, "pageUp")) return move(scroll - Math.max(1, lastHeight - 1));
          else if (matchesKey(data, "pageDown")) return move(scroll + Math.max(1, lastHeight - 1));
          else if (matchesKey(data, "home") || data === "g") return move(0);
          else if (matchesKey(data, "end") || data === "G") follow = true;
          else return;
          tui.requestRender();
        },
        handleMouse(event: { type: string; x?: number; y?: number; wheelDelta?: number }) {
          if (event.type === "wheel" && event.wheelDelta) {
            scroll = Math.max(0, scroll + event.wheelDelta);
            follow = false;
            return { handled: true };
          }
          if (event.type === "click" && event.y === tabY && event.x !== undefined) {
            const tab = tabHits.find(([x0, x1]) => event.x! >= x0 && event.x! < x1);
            if (tab) {
              setStream(tab[2]);
              return { handled: true };
            }
          }
          return undefined;
        },
        dispose() {
          disposed = true;
          if (timer !== null) clock.clearInterval(timer);
        },
      };
    },
    { overlay: true, overlayOptions: { anchor: "bottom-left", width: "100%", maxHeight: "100%", margin: 0 } },
  );
}
