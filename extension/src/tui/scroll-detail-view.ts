/**
 * Shared chrome for /tasks detail views, and the subagent detail itself.
 *
 *   ◆ ch_ab12 · scout                                           ● following
 *   ─┐¹conversation┌┐²result┌┐³info┌──────────────────────────────────────
 *   …body…                                                               ┃
 *   ⠋ working   12.4k in · 1.8k out · 24.0 tok/s · llm 1m02s · 2 shells
 *   esc close  o unfold all  f unfollow  1 2 3 tab  ↑↓ scroll  g G top / end
 *
 * Same layout and keys as `pi-famulus top`: tabs cut into a rule, a
 * follow / % pill, hints that drop the least important first.
 *
 * pi's `ui.custom()` without `overlay` replaces the editor, and ScrollView only
 * clips when the chat layout engine owns it. This panel is a full-screen overlay
 * that slices its own lines, so wheel / page keys scroll and terminal selection
 * can copy the visible text.
 */
import type { ExtensionUIContext } from "@earendil-works/pi-coding-agent";
import { fitLines, loadPiTui, truncateToWidth, visibleWidth } from "./pi-tui-load";
import { realClock, type Clock, type ClockTimer } from "../clock";

export type AgentDetailTab = "conversation" | "result" | "info";

const TAB_ORDER: AgentDetailTab[] = ["conversation", "result", "info"];

export interface PaneTheme {
  fg(color: string, text: string): string;
  bold(text: string): string;
  italic(text: string): string;
  underline?(text: string): string;
}

/** A pane that lays itself out for the body width (the chat thread). */
export interface PaneRenderer {
  render(width: number, theme: PaneTheme, state: { expanded: boolean }): string[];
}

/** Plain text (wrapped by the view) or a width-aware renderer. */
export type Pane = (() => string) | PaneRenderer;

export interface ScrollDetailOptions {
  title: string;
  /** Named panes; 1/2/3 and Tab cycle through the provided ones. */
  tabs: Partial<Record<AgentDetailTab, Pane>>;
  /** Live work: shows `● following` and the `f` key. */
  live?: () => boolean;
  /** Status line above the keys (spinner, usage). */
  status?: (theme: PaneTheme) => string;
  /** Poll for live updates while the task is running. */
  pollMs?: number;
  clock?: Clock;
}

/** Left inset of the body; the header and footer share it. */
const INSET = 2;

/** Wrap on visible width. ANSI is preserved; CJK is not split by JS length. */
export function wrapLines(text: string, width: number): string[] {
  return fitLines(text, Math.max(1, width));
}

export function visibleDetailTabs(tabs: ScrollDetailOptions["tabs"]): AgentDetailTab[] {
  return TAB_ORDER.filter((key) => Boolean(tabs[key]));
}

/**
 * `left` and `right` on one row, `right` flush to the edge. The left side is
 * truncated to make room; `right` drops only when barely anything is left.
 */
export function spread(left: string, right: string, width: number): string {
  const w = Math.max(1, width);
  const rw = visibleWidth(right);
  if (!right || w - rw - 2 < 8) return truncateToWidth(left, w, "…");
  const l = truncateToWidth(left, w - rw - 2, "…");
  return l + " ".repeat(w - visibleWidth(l) - rw) + right;
}

/** `key label` pairs: key bright, label dim. */
export function keyHints(theme: PaneTheme, pairs: ReadonlyArray<readonly [string, string]>): string {
  return pairs.map(([key, label]) => `${theme.bold(theme.fg("muted", key))} ${theme.fg("dim", label)}`).join("  ");
}

/** A hint and its priority: 0 always stays, higher numbers drop first. */
export type Key = readonly [string, string, number];

/** As many hints as fit `width` after `prefix`, least important dropped first, order kept. */
export function fitKeys(theme: PaneTheme, prefix: string, keys: readonly Key[], width: number): string {
  const keep = keys.map(() => true);
  const render = () => {
    const shown = keys.filter((_, i) => keep[i]).map(([k, l]) => [k, l] as const);
    return `${prefix}${prefix && shown.length ? "  " : ""}${keyHints(theme, shown)}`;
  };
  while (visibleWidth(render()) > width) {
    let drop = -1;
    keys.forEach(([, , p], i) => {
      if (keep[i] && p > 0 && (drop < 0 || p >= keys[drop][2])) drop = i;
    });
    if (drop < 0) break;
    keep[drop] = false;
  }
  return truncateToWidth(render(), Math.max(1, width), "…");
}

const SUPERSCRIPT = ["¹", "²", "³", "⁴"];

/** `─┐¹output┌┐²stderr┌───── right ─` and each tab's [x0, x1) column range. */
export function tabRow<T extends string>(
  tabs: readonly T[],
  active: T,
  right: string,
  theme: PaneTheme,
  width: number,
): { line: string; hits: Array<[number, number, T]> } {
  const rule = (text: string) => theme.fg("borderMuted", text);
  let line = rule("─");
  let x = 1;
  const hits: Array<[number, number, T]> = [];
  tabs.forEach((tab, index) => {
    const name = tab === active
      ? theme.bold(theme.fg("accent", theme.underline ? theme.underline(tab) : tab))
      : theme.fg("muted", tab);
    const seg = `${rule("┐")}${theme.fg("error", SUPERSCRIPT[index] ?? "")}${name}${rule("┌")}`;
    const w = visibleWidth(seg);
    hits.push([x, x + w, tab]);
    x += w;
    line += seg;
  });
  const tail = right ? `${rule("┐")}${right}${rule("┌─")}` : rule("─");
  const fill = Math.max(0, width - x - visibleWidth(tail));
  return { line: truncateToWidth(line + rule("─".repeat(fill)) + tail, Math.max(1, width), ""), hits };
}

/** Right-hand pill: % when scrolled back, `● following` while live at the end. */
export function positionPill(theme: PaneTheme, follow: boolean, live: boolean, len: number, height: number, scroll: number): string {
  if (!follow && len > height) return theme.fg("dim", `${Math.round((scroll / Math.max(1, len - height)) * 100)}%`);
  if (follow && live) return `${theme.fg("success", "●")} ${theme.fg("dim", "following")}`;
  return "";
}

/** Scroll thumb rows `[start, end)`, or null when everything fits. */
export function scrollThumb(len: number, height: number, scroll: number): { start: number; end: number } | null {
  if (len <= height || height <= 0) return null;
  const size = Math.max(1, Math.min(height, Math.floor((height * height) / len)));
  const max = len - height;
  const start = Math.min(height - size, Math.round((scroll * (height - size)) / Math.max(1, max)));
  return { start, end: start + size };
}

/** Body rows with the thumb in the last column. */
export function withThumb(lines: string[], width: number, len: number, height: number, scroll: number, theme: PaneTheme): string[] {
  const thumb = scrollThumb(len, height, scroll);
  return lines.map((line, i) => {
    const fitted = truncateToWidth(line, Math.max(1, width - 1), "…");
    if (!thumb) return fitted;
    const pad = Math.max(0, width - 1 - visibleWidth(fitted));
    return fitted + " ".repeat(pad) + (i >= thumb.start && i < thumb.end ? theme.fg("muted", "┃") : " ");
  });
}

export function paneLines(pane: Pane | undefined, width: number, theme: PaneTheme, expanded: boolean): string[] {
  if (!pane) return [];
  const inner = Math.max(1, width - INSET);
  const pad = " ".repeat(INSET);
  if (typeof pane === "function") return wrapLines(pane(), inner).map((line) => pad + line);
  return pane.render(inner, theme, { expanded }).map((line) => pad + line);
}

export async function showScrollDetail(ui: ExtensionUIContext, options: ScrollDetailOptions): Promise<void> {
  const piTui = loadPiTui();
  if (!piTui) throw new Error("pi-tui is not available");
  const { matchesKey } = piTui;
  await ui.custom<void>(
    (tui, rawTheme, _kb, done) => {
      const theme = rawTheme as unknown as PaneTheme;
      const tabKeys = () => visibleDetailTabs(options.tabs);
      let tab: AgentDetailTab = tabKeys()[0] ?? "conversation";
      let follow = true;
      let expanded = false;
      let scroll = 0;
      let timer: ClockTimer | null = null;
      const clock = options.clock ?? realClock;
      let disposed = false;
      let tabHits: Array<[number, number, AgentDetailTab]> = [];
      let lastHeight = 1;

      const pane = () => options.tabs[tab];
      const foldable = () => {
        const p = pane();
        return p !== undefined && typeof p !== "function";
      };
      const live = () => options.live?.() ?? false;
      const bodyHeight = () => Math.max(1, Math.max(8, tui.terminal?.rows ?? 24) - (2 + (options.status ? 1 : 0) + 1));
      const setTab = (next: AgentDetailTab) => {
        if (next === tab) return;
        tab = next;
        scroll = 0;
        follow = true;
      };

      if (options.pollMs && options.pollMs > 0) {
        timer = clock.setInterval(() => {
          if (!disposed) tui.requestRender();
        }, options.pollMs);
        clock.unref?.(timer);
      }

      return {
        render(width: number) {
          const height = bodyHeight();
          lastHeight = height;
          const lines = paneLines(pane(), Math.max(1, width - 1), theme, expanded);
          const max = Math.max(0, lines.length - height);
          if (follow || scroll >= max) {
            scroll = max;
            follow = true;
          }
          const head = spread(
            `${theme.fg("accent", "◆")} ${theme.bold(options.title)}`,
            positionPill(theme, follow, live(), lines.length, height, scroll),
            width,
          );
          const keys = tabKeys();
          const row = tabRow(keys, tab, "", theme, width);
          tabHits = row.hits;
          const visible = lines.slice(scroll, scroll + height);
          while (visible.length < height) visible.push("");
          const body = withThumb(visible, width, lines.length, height, scroll, theme);
          const hints: Key[] = [["esc", "close", 0]];
          if (foldable()) hints.push(["o", expanded ? "fold all" : "unfold all", 1]);
          if (live()) hints.push(["f", follow ? "unfollow" : "follow", 2]);
          if (keys.length > 1) hints.push([keys.map((_, i) => i + 1).join(" "), "tab", 3]);
          hints.push(["↑↓", "scroll", 4], ["g G", "top / end", 5]);
          const status = options.status ? [truncateToWidth(` ${options.status(theme)}`, width, "…")] : [];
          return [head, row.line, ...body, ...status, ` ${fitKeys(theme, "", hints, width - 1)}`];
        },
        invalidate() {},
        handleInput(data: string) {
          const move = (next: number) => {
            scroll = Math.max(0, next);
            follow = false;
            tui.requestRender();
          };
          if (matchesKey(data, "escape") || data === "q") {
            done();
            return;
          }
          const keys = tabKeys();
          const shortcut = /^[123]$/.test(data) ? Number(data) - 1 : -1;
          if (keys.length > 0 && (matchesKey(data, "tab") || data === "\t")) setTab(keys[(keys.indexOf(tab) + 1) % keys.length]);
          else if (shortcut >= 0 && shortcut < keys.length) setTab(keys[shortcut]);
          else if ((data === "o" || data === "O") && foldable()) expanded = !expanded;
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
          if (event.type === "click" && event.y === 1 && event.x !== undefined) {
            const hit = tabHits.find(([x0, x1]) => event.x! >= x0 && event.x! < x1);
            if (hit) {
              setTab(hit[2]);
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
    {
      overlay: true,
      overlayOptions: {
        anchor: "bottom-left",
        width: "100%",
        maxHeight: "100%",
        margin: 0,
      },
    },
  );
}

/** Fallback when pi-tui cannot load (should not happen in interactive mode). */
export function notifyPlainFallback(
  notify: (message: string, type?: "info" | "warning" | "error") => void,
  title: string,
  text: string,
): void {
  notify(`${title}\n${text.slice(0, 4000)}`, "info");
}
