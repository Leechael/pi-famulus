/**
 * One index of background work for the fleet line, /tasks, and task_list.
 *
 * Finished items stay for 10 minutes (cap 50). Sync-waited shells are never
 * inserted — only backgrounded shells count as workers.
 */
import { realClock, type Clock } from "./clock";

export type WorkKind = "shell" | "monitor" | "agent";

export const WORK_RETAIN_MS = 10 * 60 * 1000;
export const WORK_FINISHED_CAP = 50;

export interface WorkItem {
  id: string;
  kind: WorkKind;
  /** Daemon work-kind label for agents; shell commands are classified separately. */
  workKind?: string;
  /** Time queued for local/machine admission before the agent started. */
  queueMs?: number;
  status: string;
  title: string;
  command?: string;
  cwd?: string;
  startedAt: number;
  endedAt?: number;
  exitCode?: number | null;
  signal?: string;
  endReason?: string;
  outputPath?: string;
  stderrPath?: string;
  /** Backgrounded parent shell. Ignored for monitor/agent. */
  countsAsWorker: boolean;
  runId?: string;
  name?: string;
  agent?: string;
  model?: string;
  /** User-authored prompt, excluding injected agent/model preamble. */
  prompt?: string;
  /** Agent-authored preamble, shown separately from the user task prompt. */
  preamble?: string;
  /** Last assistant text, for task_output on ch_ ids. */
  text?: string;
  /** Failure reason for failed agents (shown in /tasks and task_list). */
  error?: string;
}

export interface WorkCounts {
  workers: number;
  subagents: number;
  monitors: number;
}

export interface WorkIndexOptions {
  retainMs?: number;
  finishedCap?: number;
  clock?: Clock;
}

function isActiveStatus(status: string): boolean {
  return status === "pending" || status === "running";
}

export class WorkIndex {
  private readonly items = new Map<string, WorkItem>();
  private readonly listeners = new Set<() => void>();
  private readonly retainMs: number;
  private readonly finishedCap: number;
  private readonly clock: Clock;

  constructor(opts: WorkIndexOptions = {}) {
    this.retainMs = opts.retainMs ?? WORK_RETAIN_MS;
    this.finishedCap = opts.finishedCap ?? WORK_FINISHED_CAP;
    this.clock = opts.clock ?? realClock;
  }

  private now(): number {
    return this.clock.now();
  }

  onChange(cb: () => void): () => void {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  get(id: string): WorkItem | undefined {
    return this.items.get(id);
  }

  upsert(item: WorkItem): void {
    this.items.set(item.id, item);
    this.prune();
    this.emit();
  }

  patch(id: string, patch: Partial<WorkItem>): WorkItem | undefined {
    const cur = this.items.get(id);
    if (!cur) return undefined;
    const next = { ...cur, ...patch, id: cur.id, kind: cur.kind };
    this.items.set(id, next);
    this.prune();
    this.emit();
    return next;
  }

  /**
   * Active items (oldest first) plus finished items still inside the retain
   * window (newest first), capped.
   */
  list(now = this.now()): WorkItem[] {
    this.prune(now);
    const active: WorkItem[] = [];
    const finished: WorkItem[] = [];
    for (const item of this.items.values()) {
      if (isActiveStatus(item.status)) active.push(item);
      else finished.push(item);
    }
    active.sort((a, b) => a.startedAt - b.startedAt);
    finished.sort((a, b) => (b.endedAt ?? b.startedAt) - (a.endedAt ?? a.startedAt));
    return [...active, ...finished.slice(0, this.finishedCap)];
  }

  /**
   * Shell/monitor rows still shown as live although the manager reports the
   * task ended: their exit event was lost (reconnect, a race, an older
   * manager). The caller settles them; nothing else ever would.
   */
  staleLive<T extends { task_id: string; status: string }>(tasks: readonly T[]): T[] {
    return tasks.filter((task) => {
      if (task.status === "running") return false;
      const row = this.items.get(task.task_id);
      return row !== undefined && row.kind !== "agent" && isActiveStatus(row.status);
    });
  }

  /** Running background shells, subagents, and monitors. No total. */
  counts(now = this.now()): WorkCounts {
    let workers = 0;
    let subagents = 0;
    let monitors = 0;
    for (const item of this.list(now)) {
      if (!isActiveStatus(item.status)) continue;
      if (item.kind === "shell" && item.countsAsWorker && item.status === "running") workers++;
      else if (item.kind === "agent") subagents++;
      else if (item.kind === "monitor" && item.status === "running") monitors++;
    }
    return { workers, subagents, monitors };
  }

  private prune(now = this.now()): void {
    const finished: WorkItem[] = [];
    for (const item of this.items.values()) {
      if (isActiveStatus(item.status)) continue;
      const ended = item.endedAt ?? item.startedAt;
      if (now - ended > this.retainMs) {
        this.items.delete(item.id);
        continue;
      }
      finished.push(item);
    }
    if (finished.length <= this.finishedCap) return;
    finished.sort((a, b) => (a.endedAt ?? a.startedAt) - (b.endedAt ?? b.startedAt));
    for (const item of finished.slice(0, finished.length - this.finishedCap)) {
      this.items.delete(item.id);
    }
  }

  private emit(): void {
    for (const cb of [...this.listeners]) {
      try {
        cb();
      } catch {
        // ignore listener errors
      }
    }
  }
}

export function formatAge(startedAt: number, endedAt: number | undefined, now: number): string {
  const end = endedAt ?? now;
  const ms = Math.max(0, end - startedAt);
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
  return `${Math.floor(ms / 3_600_000)}h${Math.floor((ms % 3_600_000) / 60_000)}m`;
}

/**
 * Next time the age displayed by {@link formatAge} for an item started at
 * `startedAt` changes, given at most one render per second.
 *
 * Below 1s formatAge shows raw milliseconds, so a sub-second item's first
 * scheduled update is the transition to "1s" at the 1s mark. At 1s and above
 * the value rounds to the nearest second and flips on half-second boundaries
 * relative to `startedAt`.
 */
export function nextAgeBoundary(now: number, startedAt: number): number {
  const age = now - startedAt;
  if (age < 1000) return now + (1000 - age);
  const offset = age % 1000;
  return now + (offset < 500 ? 500 - offset : 1500 - offset);
}
