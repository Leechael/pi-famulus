/**
 * The /intercom overlay: session list + compose box.
 *
 * Upstream baseline: agent-intercom @ 98183fb, plugin/pi — verbatim port of
 * the plugin's SessionListOverlay/ComposeOverlay and their text helpers.
 */

import type { KeybindingsManager, Theme } from "@earendil-works/pi-coding-agent";
import type { SessionRow } from "./types";
import { stringWidth } from "./widgets";

export type OverlayTui = { requestRender(): void };

export type OverlayComponent = {
  invalidate(): void;
  handleInput?(data: string): void;
  render(width: number): string[];
  dispose?(): void;
};

export function formatLastActive(updatedAt: number | undefined, now = Date.now()): string {
  if (!updatedAt || updatedAt <= 0) return "-";
  const elapsed = Math.max(0, now - updatedAt);
  if (elapsed < 10_000) return "now";
  if (elapsed < 60_000) return `${Math.floor(elapsed / 1_000)}s ago`;
  if (elapsed < 60 * 60_000) return `${Math.floor(elapsed / 60_000)}m ago`;
  if (elapsed < 24 * 60 * 60_000) return `${Math.floor(elapsed / (60 * 60_000))}h ago`;
  const date = new Date(updatedAt);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/**
 * Fuzzy matching ported from pi-tui's fuzzy.js so @mention completion and
 * the overlay filter behave like pi's built-in slash-command completion:
 * all query characters must appear in order (not necessarily consecutive);
 * lower score = better match.
 */
export function fuzzyMatch(query: string, text: string): { matches: boolean; score: number } {
  const queryLower = query.toLowerCase();
  const textLower = text.toLowerCase();
  if (queryLower.length === 0) return { matches: true, score: 0 };
  if (queryLower.length > textLower.length) return { matches: false, score: 0 };
  let queryIndex = 0;
  let score = 0;
  let lastMatchIndex = -1;
  let consecutiveMatches = 0;
  for (let i = 0; i < textLower.length && queryIndex < queryLower.length; i++) {
    if (textLower[i] === queryLower[queryIndex]) {
      const isWordBoundary = i === 0 || /[\s\-_./:]/.test(textLower[i - 1]);
      if (lastMatchIndex === i - 1) {
        consecutiveMatches++;
        score -= consecutiveMatches * 5;
      } else {
        consecutiveMatches = 0;
        if (lastMatchIndex >= 0) {
          score += (i - lastMatchIndex - 1) * 2;
        }
      }
      if (isWordBoundary) score -= 10;
      score += i * 0.1;
      lastMatchIndex = i;
      queryIndex++;
    }
  }
  if (queryIndex < queryLower.length) return { matches: false, score: 0 };
  if (queryLower === textLower) score -= 100;
  return { matches: true, score };
}

/** Filter and sort candidates by fuzzy match quality (best matches first). */
export function fuzzyFilter<T>(items: T[], query: string, getText: (item: T) => string): T[] {
  if (!query.trim()) return items;
  const tokens = query
    .trim()
    .split(/[\s/]+/)
    .filter((t) => t.length > 0);
  if (tokens.length === 0) return items;
  const results: { item: T; totalScore: number }[] = [];
  for (const item of items) {
    const text = getText(item);
    let totalScore = 0;
    let allMatch = true;
    for (const token of tokens) {
      const match = fuzzyMatch(token, text);
      if (match.matches) {
        totalScore += match.score;
      } else {
        allMatch = false;
        break;
      }
    }
    if (allMatch) results.push({ item, totalScore });
  }
  results.sort((a, b) => a.totalScore - b.totalScore);
  return results.map((r) => r.item);
}

export function stripAnsi(text: string): string {
  return text.replace(/\x1b\[[0-9;]*m/g, "");
}

export function visibleWidth(text: string): number {
  return stringWidth(stripAnsi(text));
}

export function truncateToWidth(text: string, maxWidth: number): string {
  if (maxWidth <= 0) return "";
  const tokens = text.match(/(\x1b\[[0-9;]*m|[\s\S])/g) ?? [];
  let out = "";
  let width = 0;
  let hasAnsi = false;
  let truncated = false;
  for (const token of tokens) {
    if (token.startsWith("\x1b")) {
      out += token;
      hasAnsi = true;
      continue;
    }
    const tokenWidth = stringWidth(token);
    if (width + tokenWidth > maxWidth) {
      truncated = true;
      break;
    }
    out += token;
    width += tokenWidth;
  }
  return truncated && hasAnsi ? `${out}\x1b[0m` : out;
}

export function middleTruncate(text: string, maxWidth: number): string {
  if (visibleWidth(text) <= maxWidth) return text;
  if (maxWidth <= 3) return truncateToWidth(text, maxWidth);

  const chars = [...text];
  const targetSideWidth = Math.max(1, Math.floor((maxWidth - 1) / 2));
  let left = "";
  for (const char of chars) {
    if (visibleWidth(left + char) > targetSideWidth) break;
    left += char;
  }

  let right = "";
  for (const char of chars.slice().reverse()) {
    if (visibleWidth(char + right) > targetSideWidth) break;
    right = char + right;
  }
  return truncateToWidth(`${left}…${right}`, maxWidth);
}

export function shortSessionId(sessionId: string | undefined): string {
  return sessionId ? sessionId.slice(0, 8) : "?";
}

export function sessionTitle(session: SessionRow, options?: { self?: boolean; sameCwd?: boolean }): string {
  const name = session.name || "Unnamed session";
  const tags = [options?.self ? "self" : undefined, options?.sameCwd ? "same cwd" : undefined]
    .filter((tag): tag is string => Boolean(tag));
  const suffix = tags.length ? ` [${tags.join(", ")}]` : "";
  return `${name} (${shortSessionId(session.id)})${suffix}`;
}

export function sessionDetails(session: SessionRow, contentWidth: number): string {
  const cwd = middleTruncate(session.cwd || "(no cwd)", Math.max(8, contentWidth - 4));
  const harness = session.harness || "unknown";
  const status = session.status ? ` • ${session.status}` : "";
  return `${cwd} • ${harness}${status} • ${formatLastActive(session.updated_at)}`;
}

export class SessionListOverlay implements OverlayComponent {
  private theme: Theme;
  private keybindings: KeybindingsManager;
  private currentSession: SessionRow;
  private done: (result: SessionRow | undefined) => void;
  private sessions: SessionRow[];
  private selectedIndex = 0;
  private maxVisible = 8;
  private filtering = false;
  private filterQuery = "";

  constructor(
    theme: Theme,
    keybindings: KeybindingsManager,
    currentSession: SessionRow,
    sessions: SessionRow[],
    done: (result: SessionRow | undefined) => void,
  ) {
    this.theme = theme;
    this.keybindings = keybindings;
    this.currentSession = currentSession;
    this.sessions = sessions;
    this.done = done;
  }

  invalidate(): void {}

  private filteredSessions(): SessionRow[] {
    if (!this.filterQuery.trim()) return this.sessions;
    return fuzzyFilter(
      this.sessions,
      this.filterQuery,
      (session) => `${session.name ?? ""} ${session.id ?? ""} ${session.harness ?? ""} ${session.status ?? ""} ${session.cwd ?? ""}`,
    );
  }

  private clampSelection(sessions: SessionRow[]): void {
    if (sessions.length === 0) {
      this.selectedIndex = 0;
      return;
    }
    this.selectedIndex = Math.min(this.selectedIndex, sessions.length - 1);
  }

  private updateFilter(query: string): void {
    this.filterQuery = query;
    this.selectedIndex = 0;
  }

  handleInput(data: string): void {
    const sessions = this.filteredSessions();

    if (this.keybindings.matches(data, "tui.select.cancel")) {
      if (this.filtering) {
        this.filtering = false;
        this.updateFilter("");
        return;
      }
      this.done(undefined);
      return;
    }

    if (!this.filtering && data === "/") {
      this.filtering = true;
      return;
    }

    if (this.keybindings.matches(data, "tui.select.up")) {
      if (sessions.length > 0) {
        this.selectedIndex = this.selectedIndex === 0 ? sessions.length - 1 : this.selectedIndex - 1;
      }
      return;
    }
    if (this.keybindings.matches(data, "tui.select.down")) {
      if (sessions.length > 0) {
        this.selectedIndex = this.selectedIndex === sessions.length - 1 ? 0 : this.selectedIndex + 1;
      }
      return;
    }
    if (this.keybindings.matches(data, "tui.select.confirm")) {
      const session = sessions[this.selectedIndex];
      if (session) this.done(session);
      return;
    }

    if (this.filtering) {
      if (this.keybindings.matches(data, "tui.editor.deleteCharBackward")) {
        this.updateFilter([...this.filterQuery].slice(0, -1).join(""));
        return;
      }
      if (data.startsWith("\x1b")) return;
      const printable = [...data].filter((char) => char >= " ").join("");
      if (printable) this.updateFilter(this.filterQuery + printable);
    }
  }

  render(width: number): string[] {
    const innerWidth = Math.max(1, Math.min(width, 88));
    if (innerWidth === 1) return [this.theme.fg("accent", "│")];

    const contentWidth = Math.max(0, innerWidth - 2);
    const footer = `${this.keybindings.getKeys("tui.select.confirm").join("/")}: Message • /: Filter • ${this.keybindings.getKeys("tui.select.cancel").join("/")}: Close`;
    const border = (text: string) => this.theme.fg("accent", text);
    const row = (text = "") => {
      const clipped = truncateToWidth(text, contentWidth);
      return `${border("│")}${clipped}${" ".repeat(Math.max(0, contentWidth - visibleWidth(clipped)))}${border("│")}`;
    };

    const lines: string[] = [];
    lines.push(border(`╭${"─".repeat(contentWidth)}╮`));
    lines.push(row(this.theme.bold(" Current Session")));
    lines.push(border(`├${"─".repeat(contentWidth)}┤`));
    lines.push(row());
    lines.push(row(`  ${this.theme.fg("dim", sessionTitle(this.currentSession, { self: true }))}`));
    lines.push(row(`  ${this.theme.fg("dim", sessionDetails(this.currentSession, contentWidth))}`));
    lines.push(row());
    lines.push(border(`├${"─".repeat(contentWidth)}┤`));
    lines.push(row(this.theme.bold(" Other Sessions")));
    if (this.filtering || this.filterQuery) {
      lines.push(row(` ${this.theme.fg("dim", "Filter:")} ${this.filterQuery}${this.filtering ? "█" : ""}`));
    }
    lines.push(row());

    const sessions = this.filteredSessions();
    this.clampSelection(sessions);
    if (sessions.length === 0) {
      const empty = this.filterQuery ? ` No sessions matching "${this.filterQuery}"` : " No other agent-intercom sessions";
      lines.push(row(this.theme.fg("dim", empty)));
    } else {
      const startIndex = Math.max(
        0,
        Math.min(this.selectedIndex - Math.floor(this.maxVisible / 2), sessions.length - this.maxVisible),
      );
      const endIndex = Math.min(startIndex + this.maxVisible, sessions.length);
      for (let index = startIndex; index < endIndex; index += 1) {
        const session = sessions[index];
        const isSelected = index === this.selectedIndex;
        const sameCwd = Boolean(session.cwd && session.cwd === this.currentSession.cwd);
        const prefix = isSelected ? this.theme.fg("accent", "→ ") : "  ";
        const title = sessionTitle(session, { sameCwd });
        lines.push(row(`${prefix}${isSelected ? this.theme.fg("accent", title) : title}`));
        lines.push(row(`  ${this.theme.fg("dim", sessionDetails(session, contentWidth))}`));
        if (index < endIndex - 1) lines.push(row());
      }
      if (startIndex > 0 || endIndex < sessions.length) {
        lines.push(row());
        lines.push(row(this.theme.fg("dim", ` ${this.selectedIndex + 1}/${sessions.length}`)));
      }
    }

    lines.push(row());
    lines.push(border(`├${"─".repeat(contentWidth)}┤`));
    lines.push(row(this.theme.fg("dim", ` ${footer}`)));
    lines.push(border(`╰${"─".repeat(contentWidth)}╯`));
    return lines;
  }
}

export interface ComposeResult {
  sent: boolean;
  messageId?: string;
  text?: string;
}

export class ComposeOverlay implements OverlayComponent {
  private tui: OverlayTui;
  private theme: Theme;
  private keybindings: KeybindingsManager;
  private target: SessionRow;
  private targetLabel: string;
  private send: (text: string) => Promise<{ messageId?: string }>;
  private done: (result: ComposeResult) => void;
  private inputBuffer = "";
  private sending = false;
  private error: string | null = null;

  constructor(
    tui: OverlayTui,
    theme: Theme,
    keybindings: KeybindingsManager,
    target: SessionRow,
    targetLabel: string,
    send: (text: string) => Promise<{ messageId?: string }>,
    done: (result: ComposeResult) => void,
  ) {
    this.tui = tui;
    this.theme = theme;
    this.keybindings = keybindings;
    this.target = target;
    this.targetLabel = targetLabel;
    this.send = send;
    this.done = done;
  }

  invalidate(): void {}

  handleInput(data: string): void {
    if (this.sending) return;
    if (this.keybindings.matches(data, "tui.select.cancel")) {
      this.done({ sent: false });
      return;
    }
    if (data.startsWith("\x1b")) return;

    if (this.keybindings.matches(data, "tui.select.confirm")) {
      if (this.inputBuffer.trim()) void this.sendMessage();
      return;
    }
    if (this.keybindings.matches(data, "tui.editor.deleteCharBackward")) {
      this.inputBuffer = [...this.inputBuffer].slice(0, -1).join("");
      this.tui.requestRender();
      return;
    }

    const printable = [...data].filter((char) => char >= " ").join("");
    if (printable) {
      this.inputBuffer += printable;
      this.tui.requestRender();
    }
  }

  private async sendMessage(): Promise<void> {
    this.sending = true;
    this.error = null;
    this.tui.requestRender();
    const text = this.inputBuffer.trim();
    try {
      const result = await this.send(text);
      this.done({ sent: true, messageId: result.messageId, text });
    } catch (error) {
      this.error = error instanceof Error ? error.message : String(error);
      this.sending = false;
      this.tui.requestRender();
    }
  }

  render(width: number): string[] {
    const innerWidth = Math.max(1, Math.min(width, 72));
    if (innerWidth === 1) return [this.theme.fg("accent", "│")];

    const contentWidth = Math.max(0, innerWidth - 2);
    const footer = `${this.keybindings.getKeys("tui.select.confirm").join("/")}: Send • ${this.keybindings.getKeys("tui.select.cancel").join("/")}: Close`;
    const border = (text: string) => this.theme.fg("accent", text);
    const row = (text = "") => {
      const clipped = truncateToWidth(text, contentWidth);
      return `${border("│")}${clipped}${" ".repeat(Math.max(0, contentWidth - visibleWidth(clipped)))}${border("│")}`;
    };

    const lines: string[] = [];
    lines.push(border(`╭${"─".repeat(contentWidth)}╮`));
    lines.push(row(this.theme.bold(` Send to: ${this.targetLabel}`)));
    lines.push(row(this.theme.fg("dim", ` ${sessionDetails(this.target, contentWidth)}`)));
    lines.push(border(`├${"─".repeat(contentWidth)}┤`));
    lines.push(row());
    if (this.sending) {
      lines.push(row(this.theme.fg("dim", " Sending...")));
    } else if (this.error) {
      lines.push(row(this.theme.fg("error", ` Error: ${this.error}`)));
      lines.push(row());
      lines.push(row(` > ${this.inputBuffer}█`));
    } else {
      lines.push(row(` > ${this.inputBuffer}█`));
    }
    lines.push(row());
    lines.push(border(`├${"─".repeat(contentWidth)}┤`));
    lines.push(row(this.theme.fg("dim", ` ${footer}`)));
    lines.push(border(`╰${"─".repeat(contentWidth)}╯`));
    return lines;
  }
}
