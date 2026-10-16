/**
 * Full-screen detail panel for /tasks.
 *
 * pi's `ui.custom()` without `overlay` replaces the editor, and ScrollView only
 * clips when the chat layout engine owns it. This panel is a full-screen overlay
 * that slices its own lines, so wheel / page keys scroll and terminal selection
 * can copy the visible text.
 */
import type { ExtensionUIContext } from "@earendil-works/pi-coding-agent";
import { fitLines, loadPiTui, truncateToWidth, visibleWidth } from "./pi-tui-load";
import { realClock, type Clock, type ClockTimer } from "../clock";

export type TaskDetailTab = "output" | "stderr" | "info" | "conversation" | "result";

const TAB_ORDER: TaskDetailTab[] = ["output", "stderr", "info", "conversation", "result"];
const TAB_LABEL: Record<TaskDetailTab, string> = {
  output: "Output",
  stderr: "Stderr",
  info: "Info",
  conversation: "Conversation",
  result: "Result",
};

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
  /** Named panes; shortcuts 1/2/3 and Tab cycle through the provided panes. */
  tabs?: Partial<Record<TaskDetailTab, Pane>>;
  /** Single-pane content. */
  content?: Pane;
  /** Poll for live updates while the task is running. */
  pollMs?: number;
  followEnd?: boolean;
  clock?: Clock;
}

/** Left inset of the body; the header and footer share it. */
const INSET = 2;

/** Wrap on visible width. ANSI is preserved; CJK is not split by JS length. */
export function wrapLines(text: string, width: number): string[] {
  return fitLines(text, Math.max(1, width));
}

export function visibleDetailTabs(tabs: ScrollDetailOptions["tabs"]): TaskDetailTab[] {
  const order = tabs?.conversation ? ["conversation", "result", "info"] as const : TAB_ORDER;
  return order.filter((key) => Boolean(tabs?.[key]));
}

export function fitTitle(title: string, width: number): string {
  return truncateToWidth(title, Math.max(1, width), "…");
}

/** `left` and `right` on one row, `right` flush to the edge when it fits. */
export function spread(left: string, right: string, width: number): string {
  const lw = visibleWidth(left);
  const rw = visibleWidth(right);
  if (!right || lw + rw + 2 > width) return truncateToWidth(left, Math.max(1, width), "…");
  return left + " ".repeat(width - lw - rw) + right;
}

/** `key label` pairs: key bright, label dim. */
export function keyHints(theme: PaneTheme, pairs: ReadonlyArray<readonly [string, string]>): string {
  return pairs.map(([key, label]) => `${theme.bold(theme.fg("muted", key))} ${theme.fg("dim", label)}`).join("  ");
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
    (tui, theme, _kb, done) => {
      const tabKeys = () => visibleDetailTabs(options.tabs);
      let tab: TaskDetailTab = tabKeys()[0] ?? "output";
      let follow = options.followEnd !== false;
      let expanded = false;
      let scroll = 0;
      let stuckToEnd = follow;
      let timer: ClockTimer | null = null;
      const clock = options.clock ?? realClock;
      let disposed = false;
      let lastLineCount = 0;

      function pane(): Pane | undefined {
        return options.tabs?.[tab] ?? options.content;
      }

      function foldable(): boolean {
        const p = pane();
        return p !== undefined && typeof p !== "function";
      }

      function bodyHeight(width: number): { lines: string[]; height: number } {
        const rows = Math.max(8, tui.terminal?.rows ?? 24);
        // title, tabs, rule above the body; rule and keys below it.
        const chrome = 1 + (options.tabs ? 1 : 0) + 1 + 2;
        const height = Math.max(1, rows - chrome);
        return { lines: paneLines(pane(), Math.max(1, width - 1), theme as PaneTheme, expanded), height };
      }

      function refresh(): void {
        if (!disposed) tui.requestRender();
      }

      if (options.pollMs && options.pollMs > 0) {
        timer = clock.setInterval(refresh, options.pollMs);
        clock.unref?.(timer);
      }

      return {
        render(width: number) {
          const { lines, height } = bodyHeight(width);
          const maxScroll = Math.max(0, lines.length - height);
          if (lines.length !== lastLineCount) {
            if (stuckToEnd) scroll = maxScroll;
            lastLineCount = lines.length;
          }
          scroll = Math.max(0, Math.min(scroll, maxScroll));
          stuckToEnd = follow && scroll >= maxScroll;

          const t = theme as PaneTheme;
          const dim = (s: string) => t.fg("dim", s);
          const rule = t.fg("borderMuted", "─".repeat(Math.max(1, width)));
          const status = lines.length <= height
            ? ""
            : stuckToEnd
              ? `${t.fg("success", "●")} ${dim("following")}`
              : dim(`${Math.round((scroll / Math.max(1, maxScroll)) * 100)}%`);
          const head = spread(`${t.fg("accent", "◆")} ${t.bold(fitTitle(options.title, Math.max(1, width - 16)))}`, status, width);
          const keys = tabKeys();
          const tabLine = keys.length > 0
            ? " ".repeat(INSET) + keys.map((key, index) => {
              const label = TAB_LABEL[key];
              const name = tab === key ? t.bold(t.fg("accent", t.underline ? t.underline(label) : label)) : t.fg("muted", label);
              return `${dim(String(index + 1))} ${name}`;
            }).join("   ")
            : "";
          const visible = lines.slice(scroll, scroll + height);
          while (visible.length < height) visible.push("");
          const thumb = scrollThumb(lines.length, height, scroll);
          const body = visible.map((line, i) => {
            if (!thumb) return line;
            const pad = Math.max(0, width - 1 - visibleWidth(line));
            return line + " ".repeat(pad) + (i >= thumb.start && i < thumb.end ? t.fg("muted", "┃") : " ");
          });
          const place = lines.length <= height ? "" : dim(`${scroll + 1}–${Math.min(lines.length, scroll + height)} of ${lines.length}`);
          const pairs: Array<[string, string]> = [["↑↓", "scroll"]];
          if (keys.length > 1) pairs.push(["tab", "switch"]);
          if (foldable()) pairs.push(["o", expanded ? "fold all" : "unfold all"]);
          pairs.push(["f", follow ? "unfollow" : "follow"], ["esc", "close"]);
          const foot = spread(" ".repeat(INSET) + keyHints(t, pairs), place, width);
          return [head, ...(tabLine ? [tabLine] : []), rule, ...body, rule, foot];
        },
        invalidate() {},
        handleInput(data: string) {
          const width = Math.max(20, tui.terminal?.columns ?? 80);
          const { lines, height } = bodyHeight(width);
          const maxScroll = Math.max(0, lines.length - height);
          const move = (next: number) => {
            scroll = Math.max(0, Math.min(maxScroll, next));
            stuckToEnd = follow && scroll >= maxScroll;
            tui.requestRender();
          };
          if (matchesKey(data, "escape") || data === "q") {
            done();
            return;
          }
          const keys = tabKeys();
          const shortcut = /^[123]$/.test(data) ? Number(data) - 1 : -1;
          if (keys.length > 0 && (matchesKey(data, "tab") || data === "\t")) {
            const current = keys.indexOf(tab);
            tab = keys[(current + 1) % keys.length];
            scroll = 0;
            stuckToEnd = follow;
            tui.requestRender();
            return;
          }
          if (shortcut >= 0 && shortcut < keys.length) {
            tab = keys[shortcut];
            scroll = 0;
            stuckToEnd = follow;
            tui.requestRender();
            return;
          }
          if ((data === "o" || data === "O") && foldable()) {
            expanded = !expanded;
            tui.requestRender();
            return;
          }
          if (data === "f" || data === "F") {
            follow = !follow;
            if (follow) scroll = maxScroll;
            stuckToEnd = follow;
            tui.requestRender();
            return;
          }
          if (matchesKey(data, "up")) move(scroll - 1);
          else if (matchesKey(data, "down")) move(scroll + 1);
          else if (matchesKey(data, "pageUp")) move(scroll - Math.max(1, height - 1));
          else if (matchesKey(data, "pageDown")) move(scroll + Math.max(1, height - 1));
          else if (matchesKey(data, "home")) move(0);
          else if (matchesKey(data, "end")) move(maxScroll);
        },
        handleMouse(event: { type: string; wheelDelta?: number }) {
          if (event.type !== "wheel" || !event.wheelDelta) return undefined;
          const width = Math.max(20, tui.terminal?.columns ?? 80);
          const { lines, height } = bodyHeight(width);
          const maxScroll = Math.max(0, lines.length - height);
          scroll = Math.max(0, Math.min(maxScroll, scroll + event.wheelDelta));
          stuckToEnd = follow && scroll >= maxScroll;
          return { handled: true };
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

/** Rows `[start, end)` of the viewport that carry the scroll thumb, or null when it all fits. */
export function scrollThumb(len: number, height: number, scroll: number): { start: number; end: number } | null {
  if (len <= height || height <= 0) return null;
  const size = Math.max(1, Math.min(height, Math.floor((height * height) / len)));
  const max = len - height;
  const start = Math.min(height - size, Math.round((scroll * (height - size)) / Math.max(1, max)));
  return { start, end: start + size };
}

/** Fallback when pi-tui cannot load (should not happen in interactive mode). */
export function notifyPlainFallback(
  notify: (message: string, type?: "info" | "warning" | "error") => void,
  title: string,
  text: string,
): void {
  notify(`${title}\n${text.slice(0, 4000)}`, "info");
}
