/**
 * Passive fleet line below the editor — live work only, no keyboard hooks.
 *
 * Format: `● 2 shells · 1 monitor · scout 12s   /tasks`
 *   shells   = backgrounded shell tasks (not sync-waited)
 *   monitors = active monitors
 *   then each running subagent by name with its age
 *
 * Driven by WorkIndex changes. Does not poll the manager.
 */
import { realClock, type Clock, type ClockTimer } from "../clock";
import { formatAge, nextAgeBoundary, type WorkIndex, type WorkItem } from "../work-index";
import { truncateToWidth } from "../tui/pi-tui-load";
import { statusGlyph } from "../tui/tool-component";

export const FLEET_WIDGET_KEY = "pi-famulus-fleet";

function isActive(item: WorkItem): boolean {
  return item.status === "pending" || item.status === "running";
}

export interface FleetTheme {
  fg(color: string, text: string): string;
}

export interface FleetTui {
  requestRender(): void;
}

export interface FleetWidgetComponent {
  render(width: number): string[];
  invalidate?(): void;
  dispose?(): void;
}

export interface FleetUi {
  setWidget(
    key: string,
    content:
      | string[]
      | undefined
      | ((tui: FleetTui, theme: FleetTheme) => FleetWidgetComponent),
    options?: { placement?: "aboveEditor" | "belowEditor" },
  ): void;
}

export interface FleetStatusDeps {
  index: WorkIndex;
  getUi: () => FleetUi | null;
  clock?: Clock;
}

function countLabel(n: number, singular: string, plural?: string): string {
  return `${n} ${n === 1 ? singular : (plural ?? `${singular}s`)}`;
}

/** Counts part of the fleet line. "shell" = a backgrounded bash command. */
export function summaryLabel(shells: number, subagents: number, monitors: number): string {
  const parts: string[] = [];
  if (shells > 0) parts.push(countLabel(shells, "shell"));
  if (subagents > 0) parts.push(countLabel(subagents, "subagent"));
  if (monitors > 0) parts.push(countLabel(monitors, "monitor"));
  return parts.join(" · ");
}

/**
 * Passive fleet status line (below editor). No setStatus, no keyboard capture, no poll.
 */
export class FleetWidget {
  private readonly deps: FleetStatusDeps;
  private started = false;
  private widgetRegistered = false;
  private tui: FleetTui | null = null;
  private unsubscribe: (() => void) | null = null;
  private ageTimer: ClockTimer | null = null;
  private ageTimerIsTimeout = false;
  private readonly clock: Clock;

  constructor(deps: FleetStatusDeps) {
    this.deps = deps;
    this.clock = deps.clock ?? realClock;
  }

  start(): void {
    if (this.started) return;
    this.started = true;
    this.unsubscribe = this.deps.index.onChange(() => this.refresh());
    this.refresh();
  }

  refresh(): void {
    const ui = this.deps.getUi();
    if (!ui) return;
    const items = this.deps.index.list(this.clock.now());
    const counts = this.deps.index.counts();
    const activeAgents = items.filter((item) => item.kind === "agent" && isActive(item));
    const total = counts.workers + counts.subagents + counts.monitors;
    // Always reschedule: an item added while a timeout is pending may have
    // an earlier boundary, and a removed item must not tick for stale state.
    if (activeAgents.length > 0) {
      this.startAgeTicker();
    } else {
      this.clearAgeTimer();
    }
    if (total === 0) {
      this.clearWidget(ui);
      return;
    }
    if (!this.widgetRegistered) {
      ui.setWidget(
        FLEET_WIDGET_KEY,
        (tui, theme) => {
          this.tui = tui;
          this.widgetRegistered = true;
          // The ticker refuses to schedule while no component is mounted
          // (refresh may have run before the host invoked the factory).
          if (!this.ageTimer && this.deps.index.list(this.clock.now()).some((item) => item.kind === "agent" && isActive(item))) {
            this.scheduleNextAgeTick();
          }
          return {
            render: (width) => this.renderLine(width, theme),
            invalidate: () => {},
            dispose: () => {
              if (this.tui === tui) {
                this.tui = null;
                this.widgetRegistered = false;
              }
            },
          };
        },
        { placement: "belowEditor" },
      );
      return;
    }
    this.tui?.requestRender();
  }

  dispose(): void {
    this.started = false;
    this.unsubscribe?.();
    this.unsubscribe = null;
    const ui = this.deps.getUi();
    this.clearAgeTimer();
    if (ui) this.clearWidget(ui);
  }

  /**
   * Tick exactly when a displayed age changes. formatAge rounds
   * (now - startedAt) to the nearest second, so each live item's visible
   * value flips on half-second boundaries relative to its own startedAt —
   * a global phase would be early/late for items started off the second.
   * Schedule the earliest pending flip and re-evaluate after every tick.
   */
  private startAgeTicker(): void {
    this.clearAgeTimer();
    this.scheduleNextAgeTick();
  }

  private scheduleNextAgeTick(): void {
    const now = this.clock.now();
    const items = this.deps.index.list(now).filter((item) => item.kind === "agent" && isActive(item));
    if (items.length === 0 || !this.tui) return;
    let next = Infinity;
    for (const item of items) {
      next = Math.min(next, nextAgeBoundary(now, item.startedAt));
    }
    this.ageTimerIsTimeout = true;
    this.ageTimer = this.clock.setTimeout(() => {
      this.ageTimer = null;
      this.ageTimerIsTimeout = false;
      this.tui?.requestRender();
      // Phases and the active set may have changed; schedule the next flip.
      this.scheduleNextAgeTick();
    }, next - now);
    this.clock.unref?.(this.ageTimer);
  }

  private clearAgeTimer(): void {
    if (!this.ageTimer) return;
    if (this.ageTimerIsTimeout) this.clock.clearTimeout(this.ageTimer);
    else this.clock.clearInterval(this.ageTimer);
    this.ageTimer = null;
    this.ageTimerIsTimeout = false;
  }

  private clearWidget(ui: FleetUi): void {
    if (this.widgetRegistered || this.tui) {
      ui.setWidget(FLEET_WIDGET_KEY, undefined, { placement: "belowEditor" });
      this.widgetRegistered = false;
      this.tui = null;
    }
  }

  /**
   * One line: `● 2 shells · 1 monitor · alpha 12s · beta 8s   /tasks`.
   * Live work only: anything that exited, failed, or was killed drops out (its
   * wake pill and /tasks already report it), and the line clears when idle.
   */
  private renderLine(width: number, theme: FleetTheme): string[] {
    const now = this.clock.now();
    const items = this.deps.index.list(now);
    const counts = this.deps.index.counts();
    const parts: string[] = [];
    const summary = summaryLabel(counts.workers, 0, counts.monitors);
    if (summary) parts.push(theme.fg("muted", summary));
    for (const item of items.filter((i) => i.kind === "agent" && isActive(i))) {
      const name = item.name ?? item.title;
      parts.push(`${name} ${theme.fg("dim", formatAge(item.startedAt, item.endedAt, now))}`);
    }
    if (parts.length === 0) return [];
    const live = statusGlyph("running");
    const line = `  ${theme.fg(live.color, live.glyph)} ${parts.join(theme.fg("dim", " · "))}   ${theme.fg("dim", "/tasks")}`;
    return [truncateToWidth(line, Math.max(1, width), "…")];
  }
}

