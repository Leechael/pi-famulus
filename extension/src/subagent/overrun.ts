/**
 * Overrun wake inputs (design doc §4.6 child lifecycle): which foreground
 * shell each child is blocked on, and the builder that turns a runner
 * overrun tick into the wake payload.
 *
 * Zero pi dependency; the output-file stat is injected so tests can drive it.
 */
import type { SubagentOverrunInfo } from "../format";
import type { OverrunShell } from "../wake";
import type { ConversationTurn } from "./types";

export interface ChildShell {
  taskId: string;
  command: string;
  startedAt: number;
  outputPath: string;
}

/** Output that changed within this window counts as growing on a first reading. */
export const OUTPUT_GROWING_WINDOW_MS = 60_000;

/**
 * Foreground shells per child. Child bash records a shell when the manager
 * starts it and clears it when the call returns or throws; a child runs at
 * most one foreground shell at a time (no background bash in children).
 */
export class ChildShellTracker {
  private readonly shells = new Map<string, ChildShell>();
  /** Output size at the previous overrun reading, per task. */
  private readonly sizes = new Map<string, number>();

  start(childId: string, shell: ChildShell): void {
    this.shells.set(childId, shell);
  }

  end(childId: string, taskId: string): void {
    if (this.shells.get(childId)?.taskId === taskId) this.shells.delete(childId);
    this.sizes.delete(taskId);
  }

  current(childId: string): ChildShell | undefined {
    return this.shells.get(childId);
  }

  /** Record a size reading and return the previous one for the same task. */
  swapSize(taskId: string, size: number): number | undefined {
    const previous = this.sizes.get(taskId);
    this.sizes.set(taskId, size);
    return previous;
  }
}

/** What the runner knows at an overrun tick. */
export interface OverrunTick {
  childId: string;
  elapsedMs: number;
  budgetMs: number;
  reminder: number;
  nextReminderMs: number;
  lastEventAt: number;
}

export interface OverrunInfoDeps {
  now: () => number;
  /** Run id and display name; undefined once the child's run is gone. */
  lookupChild: (childId: string) => { runId: string; name: string } | undefined;
  conversation: (childId: string) => ConversationTurn[];
  shells: ChildShellTracker;
  /** Output file size and mtime; null when it cannot be read. */
  stat: (path: string) => { size: number; mtimeMs: number } | null;
}

function lastActivityText(turns: ConversationTurn[]): string {
  const last = turns[turns.length - 1];
  return last ? `${last.role}: ${last.text}` : "";
}

function describeShell(deps: OverrunInfoDeps, shell: ChildShell): OverrunShell {
  const now = deps.now();
  const st = deps.stat(shell.outputPath);
  let outputBytes: number | null = null;
  let outputIdleMs: number | null = null;
  let growing: boolean | null = null;
  if (st) {
    outputBytes = st.size;
    outputIdleMs = Math.max(0, now - st.mtimeMs);
    const previous = deps.shells.swapSize(shell.taskId, st.size);
    growing = (previous !== undefined && st.size > previous) || outputIdleMs < OUTPUT_GROWING_WINDOW_MS;
  }
  return {
    taskId: shell.taskId,
    command: shell.command,
    elapsedMs: Math.max(0, now - shell.startedAt),
    outputPath: shell.outputPath,
    outputBytes,
    outputIdleMs,
    growing,
  };
}

export function buildOverrunInfo(deps: OverrunInfoDeps, tick: OverrunTick): SubagentOverrunInfo | undefined {
  const child = deps.lookupChild(tick.childId);
  if (!child) return undefined;
  const shell = deps.shells.current(tick.childId);
  return {
    runId: child.runId,
    childId: tick.childId,
    name: child.name,
    elapsedMs: tick.elapsedMs,
    budgetMs: tick.budgetMs,
    reminder: tick.reminder,
    nextReminderMs: tick.nextReminderMs,
    lastActivity: {
      agoMs: Math.max(0, deps.now() - tick.lastEventAt),
      text: lastActivityText(deps.conversation(tick.childId)),
    },
    ...(shell ? { shell: describeShell(deps, shell) } : {}),
  };
}
