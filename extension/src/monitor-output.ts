import { LineBatcher } from "./monitor-batching";
import { realClock, type Clock } from "./clock";
import type { MonitorOutputRecord } from "./manager-client";
import type { MonitorOutputInfo } from "./wake";

export const DEFAULT_DEDUP_WINDOW_MS = 60_000;
export const MAX_DEDUP_KEYS = 2048;
export type FingerprintMode = "exact" | "ignore-timestamps";
export interface MonitorOutputBatch { text: string; output: MonitorOutputInfo }
interface Identity { phase: "buffered" | "queued" | "emitted"; lastEmitted?: number; preview: string }

/** Merge original-byte range envelopes, not a promise of contiguous identical events. */
export function mergeOutputInfo(a: MonitorOutputInfo, b: MonitorOutputInfo): MonitorOutputInfo {
  const min = (x?: number, y?: number) => x === undefined ? y : y === undefined ? x : Math.min(x, y);
  const max = (x?: number, y?: number) => x === undefined ? y : y === undefined ? x : Math.max(x, y);
  return {
    firstObservedAt: min(a.firstObservedAt, b.firstObservedAt),
    lastObservedAt: max(a.lastObservedAt, b.lastObservedAt),
    cursorStart: min(a.cursorStart, b.cursorStart), cursorEnd: max(a.cursorEnd, b.cursorEnd),
    truncated: a.truncated || b.truncated,
    suppressedCount: (a.suppressedCount ?? 0) + (b.suppressedCount ?? 0),
    omittedChars: (a.omittedChars ?? 0) + (b.omittedChars ?? 0),
  };
}

/**
 * Rust owns framing and hashing; this module owns the notification window.
 * Pending identities are suppressed until onDelivered starts their fixed window.
 * No timers send repeat-only summaries. Original bytes stay in the manager log.
 */
export class MonitorOutputBuffer {
  private readonly clock: Clock;
  private readonly batcher: LineBatcher;
  private readonly identities = new Map<string, Identity>();
  private batchKeys = new Set<string>();
  private batchOutput: MonitorOutputInfo = {};
  private suppressed: MonitorOutputInfo = {};
  private disposed = false;
  private lastNow = 0;

  constructor(private readonly options: {
    clock?: Clock;
    windowMs?: number;
    mode?: FingerprintMode;
    onFlush: (batch: MonitorOutputBatch) => boolean;
    onSuppressed?: () => void;
  }) {
    this.clock = options.clock ?? realClock;
    this.batcher = new LineBatcher({ clock: this.clock, onFlush: (text, info) => {
      this.batchOutput = mergeOutputInfo(this.batchOutput, info);
      this.emit(text);
    } });
  }

  /** Stable callback identity: NotifyCenter retains it once per pending monitor. */
  readonly onDelivered = (visiblePreview?: string): void => {
    const now = this.now();
    const visibleLines = visiblePreview === undefined ? undefined : new Set(visiblePreview ? visiblePreview.split("\n") : []);
    for (const [key, identity] of this.identities) {
      if (identity.phase !== "queued") continue;
      // Both local batching and busy coalescing can omit an entire identity.
      // Never pretend the model saw it: truncated representatives fail open.
      if (visibleLines && !visibleLines.has(identity.preview)) {
        if (identity.lastEmitted === undefined) this.identities.delete(key);
        else identity.phase = "emitted";
      } else {
        identity.phase = "emitted";
        identity.lastEmitted = now;
      }
    }
  };

  push(record: MonitorOutputRecord): void {
    if (this.disposed || !record.preview) return;
    const now = this.now();
    const info: MonitorOutputInfo = {
      firstObservedAt: record.observed_at, lastObservedAt: record.observed_at,
      cursorStart: record.cursor_start, cursorEnd: record.cursor_end, truncated: record.truncated,
    };
    const hash = record.kind === "line"
      ? this.options.mode === "ignore-timestamps" ? record.timestamp_fingerprint : record.fingerprint
      : undefined;
    const window = this.options.windowMs ?? DEFAULT_DEDUP_WINDOW_MS;
    const key = window > 0 ? hash : undefined;
    if (key) {
      const previous = this.identities.get(key);
      if (previous && (previous.phase !== "emitted" || now - previous.lastEmitted! < window)) {
        this.suppressed = mergeOutputInfo(this.suppressed, { ...info, suppressedCount: 1 });
        this.options.onSuppressed?.();
        return;
      }
      // Capacity eviction fails open: extra output is preferable to false suppression.
      if (!previous && this.identities.size >= MAX_DEDUP_KEYS) {
        const oldest = this.identities.keys().next().value;
        if (oldest !== undefined) {
          this.identities.delete(oldest);
          this.batchKeys.delete(oldest);
        }
      }
      this.identities.set(key, { phase: "buffered", lastEmitted: previous?.lastEmitted, preview: record.preview });
      this.batchKeys.add(key);
    }
    this.batchOutput = mergeOutputInfo(mergeOutputInfo(this.batchOutput, this.suppressed), info);
    this.suppressed = {};
    this.batcher.push(`${record.preview}\n`);
  }

  flush(): void { this.batcher.flush(); }

  /** Attach unreported repeats to the terminal notice without another event/wake. */
  takeSummary(): MonitorOutputInfo {
    const summary = this.suppressed;
    this.suppressed = {};
    return summary;
  }

  dispose(): void {
    this.disposed = true;
    this.batcher.dispose();
    this.identities.clear();
    this.batchKeys.clear();
  }

  private now(): number {
    this.lastNow = Math.max(this.lastNow, this.clock.now());
    return this.lastNow;
  }

  private emit(text: string): void {
    const keys = this.batchKeys;
    const output = this.batchOutput;
    this.batchKeys = new Set();
    this.batchOutput = {};
    for (const key of keys) {
      const identity = this.identities.get(key);
      if (identity?.phase === "buffered") identity.phase = "queued";
    }
    if (!this.options.onFlush({ text, output })) {
      // A rate-limited batch wasn't delivered: don't start a suppression window.
      for (const key of keys) {
        const identity = this.identities.get(key);
        if (identity?.phase !== "queued") continue;
        if (identity.lastEmitted === undefined) this.identities.delete(key);
        else identity.phase = "emitted";
      }
      this.suppressed = mergeOutputInfo(this.suppressed, { ...output, suppressedCount: output.suppressedCount ?? 0 });
    }
  }
}
