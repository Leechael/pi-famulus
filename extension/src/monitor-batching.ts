/**
 * Monitor output batching primitives (design doc §4.4, Appendix A).
 *
 * LineBatcher turns arbitrary output chunks into batched line payloads:
 * chunks are split on newlines, complete lines are capped per line, and
 * emissions are coalesced over a 200ms window with a per-batch size cap.
 *
 * RateLimiter is a token bucket used to throttle monitor event injection.
 */
import { realClock, type Clock, type ClockTimer } from "./clock";

export interface LineBatcherOptions {
  flushMs?: number; // batching window, default 200
  maxLineChars?: number; // per-line cap, default 500
  maxBatchChars?: number; // per-batch cap, default 3000
  onFlush: (text: string, info: { truncated: boolean; omittedChars: number }) => void;
  clock?: Clock;
}

function sourceOfSlice(text: string, source: number, start: number, end: number): number {
  if (text.length === 0 || end <= start) return 0;
  return Math.round(source * ((end - start) / text.length));
}

/** Fixed-memory head/tail preview. Counts omitted source units, not generated ellipses. */
export class BoundedPreview {
  private head = "";
  private tail = "";
  private sourceHead = 0;
  private sourceTail = 0;
  total = 0;
  private sourceTotal = 0;
  constructor(readonly limit: number) {
    if (!Number.isSafeInteger(limit) || limit < 1) throw new RangeError("preview limit must be positive");
  }
  append(text: string, sourceChars = text.length): void {
    const headLimit = Math.ceil((this.limit - 1) / 2);
    if (this.total + text.length <= this.limit) {
      this.head += text;
      this.sourceHead += sourceChars;
    } else {
      const old = this.head;
      const oldSourceHead = this.sourceHead;
      if (this.total <= this.limit) {
        const combined = old + text;
        const combinedSource = oldSourceHead + sourceChars;
        this.head = combined.slice(0, headLimit);
        this.sourceHead = sourceOfSlice(combined, combinedSource, 0, this.head.length);
      }
      const tailLimit = this.limit - 1 - headLimit;
      if (tailLimit > 0) {
        const from = (this.tail || old).concat(text);
        const fromSource = (this.tail ? this.sourceTail : oldSourceHead) + sourceChars;
        this.tail = from.slice(-tailLimit);
        this.sourceTail = sourceOfSlice(from, fromSource, from.length - this.tail.length, from.length);
      } else {
        this.tail = "";
        this.sourceTail = 0;
      }
    }
    this.total += text.length;
    this.sourceTotal += sourceChars;
  }
  get omittedChars(): number {
    if (this.total <= this.limit) return 0;
    return Math.max(0, this.sourceTotal - this.sourceHead - this.sourceTail);
  }
  get text(): string { return this.total > this.limit ? `${this.head}…${this.tail}` : this.head; }
}

export class LineBatcher {
  private readonly flushMs: number;
  private readonly maxLineChars: number;
  private readonly maxBatchChars: number;
  private readonly onFlush: LineBatcherOptions["onFlush"];
  private readonly clock: Clock;
  private partial: BoundedPreview;
  private lines: BoundedPreview;
  private lineCount = 0;
  private omittedLineChars = 0;
  private timer: ClockTimer | null = null;
  private disposed = false;

  constructor(opts: LineBatcherOptions) {
    this.flushMs = opts.flushMs ?? 200;
    this.maxLineChars = opts.maxLineChars ?? 500;
    this.maxBatchChars = opts.maxBatchChars ?? 3000;
    this.onFlush = opts.onFlush;
    this.clock = opts.clock ?? realClock;
    this.partial = new BoundedPreview(this.maxLineChars);
    this.lines = new BoundedPreview(this.maxBatchChars);
  }

  /** Bound retained data during ingestion, not just after the timer fires. */
  push(chunk: string): void {
    if (this.disposed || chunk.length === 0) return;
    const parts = chunk.split("\n");
    for (let i = 0; i < parts.length; i++) {
      this.partial.append(parts[i]);
      if (i < parts.length - 1) this.finishLine();
    }
    this.schedule();
  }

  flush(): void {
    if (this.disposed) return;
    this.clearTimer();
    this.emit();
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimer();
    this.reset();
  }

  private finishLine(): void {
    if (this.lineCount++ > 0) this.lines.append("\n", 1);
    const displayed = this.partial.text;
    this.lines.append(displayed, this.partial.total - this.partial.omittedChars);
    this.omittedLineChars += this.partial.omittedChars;
    this.partial = new BoundedPreview(this.maxLineChars);
  }

  private reset(): void {
    this.partial = new BoundedPreview(this.maxLineChars);
    this.lines = new BoundedPreview(this.maxBatchChars);
    this.lineCount = 0;
    this.omittedLineChars = 0;
  }

  private schedule(): void {
    if (this.timer !== null) return;
    this.timer = this.clock.setTimeout(() => {
      this.timer = null;
      this.emit();
    }, this.flushMs);
    this.clock.unref?.(this.timer);
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      this.clock.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private emit(): void {
    if (this.partial.total > 0) this.finishLine();
    const text = this.lines.text;
    const omittedChars = this.omittedLineChars + this.lines.omittedChars;
    this.reset();
    if (text.length > 0) this.onFlush(text, { truncated: omittedChars > 0, omittedChars });
  }
}

export interface SaturationWindowOptions {
  windowMs: number;
  dropRatio: number;
  minimumBatches: number;
}

/** Tracks whether a rolling window has a sustained ratio of dropped batches. */
export class SaturationWindow {
  private readonly windowMs: number;
  private readonly dropRatio: number;
  private readonly minimumBatches: number;
  private samples: { at: number; dropped: boolean }[] = [];

  constructor(opts: SaturationWindowOptions) {
    this.windowMs = opts.windowMs;
    this.dropRatio = opts.dropRatio;
    this.minimumBatches = opts.minimumBatches;
  }

  record(dropped: boolean, at: number): void {
    this.prune(at);
    this.samples.push({ at, dropped });
  }

  isSaturated(now: number): boolean {
    this.prune(now);
    if (this.samples.length < this.minimumBatches) return false;
    const first = this.samples[0];
    // Require the retained samples to span a full window; sparse bursts alone
    // should not be treated as sustained saturation.
    if (!first || now - first.at < this.windowMs) return false;
    const dropped = this.samples.reduce((count, sample) => count + Number(sample.dropped), 0);
    return dropped / this.samples.length >= this.dropRatio;
  }

  private prune(now: number): void {
    const cutoff = now - this.windowMs;
    let firstLive = 0;
    while (firstLive < this.samples.length && this.samples[firstLive].at < cutoff) firstLive++;
    if (firstLive > 0) this.samples = this.samples.slice(firstLive);
  }
}

export interface RateLimiterOptions {
  capacity?: number; // default 10
  refillIntervalMs?: number; // default 2000
  refillAmount?: number; // default 1
  clock?: Clock;
}

export class RateLimiter {
  private readonly capacity: number;
  private readonly refillAmount: number;
  private tokens: number;
  private readonly interval: ClockTimer;
  private readonly clock: Clock;
  private disposed = false;

  constructor(opts?: RateLimiterOptions) {
    this.capacity = opts?.capacity ?? 10;
    this.refillAmount = opts?.refillAmount ?? 1;
    const refillIntervalMs = opts?.refillIntervalMs ?? 2000;
    this.clock = opts?.clock ?? realClock;
    this.tokens = this.capacity;
    this.interval = this.clock.setInterval(() => {
      this.tokens = Math.min(this.capacity, this.tokens + this.refillAmount);
    }, refillIntervalMs);
    this.clock.unref?.(this.interval);
  }

  /** Consume `n` tokens (default 1); returns false when insufficient. */
  tryConsume(n: number = 1): boolean {
    if (this.disposed) return false;
    if (this.tokens < n) return false;
    this.tokens -= n;
    return true;
  }

  dispose(): void {
    this.disposed = true;
    this.clock.clearInterval(this.interval);
  }
}
