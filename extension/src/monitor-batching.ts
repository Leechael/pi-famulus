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
  private buckets: { sec: number; total: number; dropped: number }[] = [];
  private firstAt: number | undefined;

  constructor(opts: SaturationWindowOptions) {
    this.windowMs = opts.windowMs;
    this.dropRatio = opts.dropRatio;
    this.minimumBatches = opts.minimumBatches;
  }

  record(dropped: boolean, at: number): void {
    if (this.firstAt === undefined) this.firstAt = at;
    const sec = Math.floor(at / 1000);
    const last = this.buckets[this.buckets.length - 1];
    if (last && last.sec === sec) {
      last.total += 1;
      if (dropped) last.dropped += 1;
    } else {
      this.buckets.push({ sec, total: 1, dropped: dropped ? 1 : 0 });
    }
    this.prune(at);
  }

  isSaturated(now: number): boolean {
    this.prune(now);
    if (this.firstAt === undefined || now - this.firstAt < this.windowMs) return false;
    const cutoffSec = Math.floor((now - this.windowMs) / 1000);
    let total = 0;
    let dropped = 0;
    for (const bucket of this.buckets) {
      if (bucket.sec >= cutoffSec) {
        total += bucket.total;
        dropped += bucket.dropped;
      }
    }
    if (total < this.minimumBatches) return false;
    return dropped / total >= this.dropRatio;
  }

  bucketCountForTest(): number {
    return this.buckets.length;
  }

  private prune(now: number): void {
    const cutoffSec = Math.floor((now - this.windowMs) / 1000) - 1;
    while (this.buckets.length > 0 && this.buckets[0].sec < cutoffSec) this.buckets.shift();
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

/** Bound retained incomplete legacy/raw line across chunks. */
export const LEGACY_PARTIAL_MAX_CHARS = 500;

export class LegacyLineAssembler {
  private text = "";
  private bytes = 0;
  private start: number | undefined;

  retainedCharsForTest(): number {
    return this.text.length;
  }

  /** Drop a retained partial: metadata owns those bytes, or the next raw cursor is discontinuous. */
  reset(): void {
    this.text = "";
    this.bytes = 0;
    this.start = undefined;
  }

  push(chunk: string, chunkBytes: number, cursorStart: number, cursorEnd: number): OverloadInput | undefined {
    if (chunk.length === 0 && chunkBytes === 0) return undefined;
    if (this.start !== undefined && cursorStart !== this.start + this.bytes) this.reset();
    const savedStart = this.start ?? cursorStart;
    if (this.start === undefined) this.start = cursorStart;
    const parts = chunk.split("\n");
    if (parts.length === 1) {
      this.bytes += chunkBytes;
      if (this.text.length < LEGACY_PARTIAL_MAX_CHARS) {
        this.text = `${this.text}${chunk}`.slice(0, LEGACY_PARTIAL_MAX_CHARS);
      }
      return undefined;
    }
    const completeLines = parts.slice(0, -1);
    const remainder = parts[parts.length - 1] ?? "";
    const lines: string[] = [];
    const first = `${this.text}${completeLines[0] ?? ""}`;
    lines.push(first.length > LEGACY_PARTIAL_MAX_CHARS ? first.slice(0, LEGACY_PARTIAL_MAX_CHARS) : first);
    for (let i = 1; i < completeLines.length; i++) {
      const line = completeLines[i];
      lines.push(line.length > LEGACY_PARTIAL_MAX_CHARS ? line.slice(0, LEGACY_PARTIAL_MAX_CHARS) : line);
    }
    const remainderBytes = Buffer.byteLength(remainder);
    const completeBytes = this.bytes + chunkBytes - remainderBytes;
    this.text = remainder.slice(0, LEGACY_PARTIAL_MAX_CHARS);
    this.bytes = remainderBytes;
    this.start = remainderBytes === 0 ? undefined : cursorEnd - remainderBytes;
    return {
      lines: lines.length,
      bytes: completeBytes,
      text: lines.join("\n"),
      cursorStart: savedStart,
      cursorEnd: remainderBytes === 0 ? cursorEnd : cursorEnd - remainderBytes,
    };
  }
}

/** Rolling window for overload line-rate / drop-ratio. */
export const OVERLOAD_WINDOW_MS = 30_000;
/**
 * Draft / owner-unconfirmed. Calibrated only from one 2026-10-29 incident
 * (events-only; accepted input bytes/lines were unavailable).
 */
export const OVERLOAD_SUMMARY_LINES_PER_SEC = 5;
/** Unverified safety backstop — not calibrated. */
export const OVERLOAD_STOP_LINES_PER_SEC = 1000;
export const OVERLOAD_STOP_SUSTAIN_MS = 60_000;
export const OVERLOAD_EXIT_QUIET_MS = 60_000;
export const SUMMARY_CADENCE_MS = [30_000, 60_000, 120_000, 300_000];
export const SUMMARY_MAX_CHARS = 1500;
export const SUMMARY_MAX_SIGNATURES = 64;
export const SUMMARY_SIGNATURE_CHARS = 120;

export function normalizeSignature(line: string): string {
  let s = line;
  s = s.replace(/[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g, "");
  s = s.replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?/g, "");
  s = s.replace(/\d{2}\/[A-Za-z]{3}\/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4}/g, "");
  s = s.replace(/\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?/g, "");
  s = s.replace(/\b[A-Za-z]{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\b/g, "");
  s = stripIpAddresses(s);
  s = s.replace(/\b[0-9a-fA-F]{8,}\b/g, "");
  s = s.replace(/\d{4,}/g, "");
  s = s.replace(/\s+/g, " ").trim();
  return s.length > SUMMARY_SIGNATURE_CHARS ? s.slice(0, SUMMARY_SIGNATURE_CHARS) : s;
}

/** Strip IPv4 and IPv6 (full, compressed, bracketed, optional port) without touching HTTP 500/502 or `a:b:c`. */
function stripIpAddresses(s: string): string {
  s = s.replace(/\[[0-9a-fA-F:]+\](?::\d{1,5})?/gi, "");
  s = s.replace(/\b(?:\d{1,3}\.){3}\d{1,3}\b/g, "");
  s = s.replace(/\b(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}\b/gi, "");
  // Compressed forms must contain "::". A hextet immediately before "::" has no
  // trailing colon, so `2001:db8::1` is not parsed as `db8:` + `:1`.
  s = s.replace(
    /(?:^|[\s,;=("'])((?:[0-9a-fA-F]{1,4}:)*[0-9a-fA-F]{1,4}::(?:[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4})*)?|::(?:[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4})*)?)/gi,
    (all, addr: string) => all.slice(0, all.length - addr.length),
  );
  return s;
}

export interface OverloadSummaryOutput {
  firstObservedAt?: number;
  lastObservedAt?: number;
  cursorStart?: number;
  cursorEnd?: number;
}

export type OverloadEnterReason = "drop-ratio" | "line-rate" | "drop-ratio+line-rate";

export interface OverloadControllerOptions {
  clock?: Clock;
  windowMs?: number;
  summaryLinesPerSec?: number;
  dropRatio?: number;
  minimumBatches?: number;
  stopLinesPerSec?: number;
  stopSustainMs?: number;
  exitQuietMs?: number;
  cadenceMs?: number[];
  maxChars?: number;
  maxSignatures?: number;
  onSummary?: (text: string, output: OverloadSummaryOutput) => void;
  onEnter?: (reason: OverloadEnterReason) => void;
  onExit?: () => void;
}

export interface OverloadInput {
  lines: number;
  bytes: number;
  text?: string;
  cursorStart?: number;
  cursorEnd?: number;
}

/**
 * Overload policy: summary on rolling input line-rate or drop-ratio;
 * stop only on unverified extreme sustained line rate.
 *
 * INPUT (line-rate / stop / interval bytes) is complete raw records (`kind:"line"`)
 * or newline-terminated chunks counted in acceptOutput, before the token bucket.
 * Later-deduped lines still count as input; dedup does not consume tokens or the drop budget.
 * Interval signature stats begin at summary-mode entry (line-rate trigger included;
 * drop-ratio entry starts empty), then reset on each emitted summary.
 */
export class OverloadController {
  private readonly clock: Clock;
  private readonly windowMs: number;
  private readonly summaryLinesPerSec: number;
  private readonly stopLinesPerSec: number;
  private readonly stopSustainMs: number;
  private readonly exitQuietMs: number;
  private readonly cadenceMs: number[];
  private readonly maxChars: number;
  private readonly maxSignatures: number;
  private readonly onSummary?: OverloadControllerOptions["onSummary"];
  private readonly onEnter?: OverloadControllerOptions["onEnter"];
  private readonly onExit?: OverloadControllerOptions["onExit"];
  private readonly saturation: SaturationWindow;
  private readonly lineBuckets: { sec: number; n: number; firstAt: number }[] = [];
  private signatures = new Map<string, { count: number; extra: number }>();
  private previousSigs = new Set<string>();
  private sigSeq = 0;
  private interval = emptyInterval();
  private firstInputAt: number | undefined;
  private summaryMode = false;
  private cadenceIndex = 0;
  private belowSince: number | undefined;
  private cadenceTimer: ClockTimer | null = null;
  private exitTimer: ClockTimer | null = null;
  private watchTimer: ClockTimer | null = null;
  private disposed = false;

  constructor(opts: OverloadControllerOptions = {}) {
    this.clock = opts.clock ?? realClock;
    this.windowMs = opts.windowMs ?? OVERLOAD_WINDOW_MS;
    this.summaryLinesPerSec = opts.summaryLinesPerSec ?? OVERLOAD_SUMMARY_LINES_PER_SEC;
    this.stopLinesPerSec = opts.stopLinesPerSec ?? OVERLOAD_STOP_LINES_PER_SEC;
    this.stopSustainMs = opts.stopSustainMs ?? OVERLOAD_STOP_SUSTAIN_MS;
    this.exitQuietMs = opts.exitQuietMs ?? OVERLOAD_EXIT_QUIET_MS;
    this.cadenceMs = opts.cadenceMs ?? [...SUMMARY_CADENCE_MS];
    this.maxChars = opts.maxChars ?? SUMMARY_MAX_CHARS;
    this.maxSignatures = opts.maxSignatures ?? SUMMARY_MAX_SIGNATURES;
    this.onSummary = opts.onSummary;
    this.onEnter = opts.onEnter;
    this.onExit = opts.onExit;
    this.saturation = new SaturationWindow({
      windowMs: this.windowMs,
      dropRatio: opts.dropRatio ?? 0.5,
      minimumBatches: opts.minimumBatches ?? 10,
    });
  }

  inSummary(): boolean {
    return this.summaryMode;
  }

  shouldStop(): boolean {
    return this.sustainedStop(this.clock.now());
  }

  stopNotice(): string {
    const secs = Math.round(this.stopSustainMs / 1000);
    return `[Monitor stopped: input exceeded ~${this.stopLinesPerSec} lines/s for ${secs}s (unverified safety backstop).]`;
  }

  recordInput(input: OverloadInput): void {
    if (this.disposed) return;
    const now = this.clock.now();
    const lines = Math.max(0, input.lines);
    if (lines > 0) this.addLineBucket(lines, now);
    const wasSummary = this.summaryMode;
    if (wasSummary) {
      if (lines > 0 || input.bytes > 0) this.addInterval(input, now);
      this.evaluateExit(now);
      return;
    }
    this.maybeEnter(now);
    // Line-rate entry includes the triggering input; drop-ratio entry stays empty.
    if (this.summaryMode && this.lineRateHigh(now) && (lines > 0 || input.bytes > 0)) {
      this.addInterval(input, now);
    }
    this.evaluateExit(now);
  }

  recordBatch(dropped: boolean): void {
    if (this.disposed) return;
    const now = this.clock.now();
    this.saturation.record(dropped, now);
    this.maybeEnter(now);
    this.evaluateExit(now);
  }

  /** Emit a non-empty interval immediately (terminal notices), without advancing cadence. */
  flushSummary(): void {
    if (this.disposed || !this.summaryMode || this.interval.lines <= 0) return;
    this.emitSummary();
  }

  signatureStateForTest(): Map<string, number> {
    return new Map([...this.signatures].map(([k, v]) => [k, v.count]));
  }

  bucketCountForTest(): number {
    return this.lineBuckets.length;
  }

  dropBucketCountForTest(): number {
    return this.saturation.bucketCountForTest();
  }

  dispose(): void {
    this.disposed = true;
    this.clearCadence();
    this.clearExit();
    this.clearWatch();
  }

  private maybeEnter(now: number): void {
    if (this.summaryMode) return;
    const drop = this.saturation.isSaturated(now);
    const rate = this.lineRateHigh(now);
    if (!drop && !rate) return;
    this.summaryMode = true;
    this.cadenceIndex = 0;
    this.belowSince = undefined;
    this.clearExit();
    this.armCadence(this.cadenceMs[0] ?? 30_000);
    this.armWatch();
    const reason: OverloadEnterReason = drop && rate ? "drop-ratio+line-rate" : drop ? "drop-ratio" : "line-rate";
    this.onEnter?.(reason);
  }

  private evaluateExit(now: number): void {
    if (!this.summaryMode) return;
    const busy = this.lineRateHigh(now) || this.saturation.isSaturated(now);
    if (busy) {
      this.belowSince = undefined;
      this.clearExit();
      return;
    }
    if (this.belowSince === undefined) {
      this.belowSince = now;
      this.armExit(this.exitQuietMs);
    }
  }

  private leaveSummary(): void {
    if (this.interval.lines > 0) this.emitSummary();
    this.onExit?.();
    this.summaryMode = false;
    this.cadenceIndex = 0;
    this.belowSince = undefined;
    this.interval = emptyInterval();
    this.signatures = new Map();
    this.previousSigs = new Set();
    this.clearCadence();
    this.clearExit();
    this.clearWatch();
  }

  private onCadenceDue(): void {
    this.cadenceTimer = null;
    if (this.disposed || !this.summaryMode) return;
    this.evaluateExit(this.clock.now());
    if (!this.summaryMode) return;
    if (this.interval.lines > 0) {
      this.emitSummary();
      this.cadenceIndex = Math.min(this.cadenceIndex + 1, this.cadenceMs.length - 1);
      this.armCadence(this.cadenceMs[this.cadenceIndex] ?? 300_000);
    } else {
      this.armCadence(this.cadenceMs[this.cadenceIndex] ?? 30_000);
    }
  }

  private emitSummary(): void {
    let newSignatures = 0;
    for (const key of this.signatures.keys()) {
      if (!this.previousSigs.has(key)) newSignatures += 1;
    }
    const text = formatOverloadSummary(this.interval, this.signatures, this.maxChars, newSignatures);
    const output: OverloadSummaryOutput = {
      firstObservedAt: this.interval.firstAt,
      lastObservedAt: this.interval.lastAt,
      cursorStart: this.interval.cursorStart,
      cursorEnd: this.interval.cursorEnd,
    };
    this.previousSigs = new Set(this.signatures.keys());
    this.interval = emptyInterval();
    this.signatures = new Map();
    this.onSummary?.(text, output);
  }

  private addLineBucket(n: number, now: number): void {
    if (this.firstInputAt === undefined) this.firstInputAt = now;
    const sec = Math.floor(now / 1000);
    const last = this.lineBuckets[this.lineBuckets.length - 1];
    if (last && last.sec === sec) {
      const wasBelowStop = last.n < this.stopLinesPerSec;
      last.n += n;
      // Earlier low-rate input in this second is not the start of the flood.
      if (wasBelowStop && last.n >= this.stopLinesPerSec) last.firstAt = now;
    } else {
      this.lineBuckets.push({ sec, n, firstAt: now });
    }
    const cutoffSec = Math.floor((now - Math.max(this.windowMs, this.stopSustainMs)) / 1000) - 1;
    while (this.lineBuckets.length > 0 && this.lineBuckets[0].sec < cutoffSec) this.lineBuckets.shift();
  }

  /** Average over the last `window` completed seconds; the current partial second is excluded. */
  private lineRate(now: number): number {
    const windowSecs = Math.max(1, Math.round(this.windowMs / 1000));
    const currentSec = Math.floor(now / 1000);
    const fromSec = currentSec - windowSecs;
    const toSec = currentSec - 1;
    let n = 0;
    for (const b of this.lineBuckets) {
      if (b.sec >= fromSec && b.sec <= toSec) n += b.n;
    }
    return n / windowSecs;
  }

  /** Rolling average over the nominal window; a single burst does not fill the window. */
  private lineRateHigh(now: number): boolean {
    if (this.firstInputAt === undefined || now - this.firstInputAt < this.windowMs) return false;
    return this.lineRate(now) > this.summaryLinesPerSec;
  }

  private sustainedStop(now: number): boolean {
    const need = Math.max(1, Math.round(this.stopSustainMs / 1000));
    const currentSec = Math.floor(now / 1000);
    const from = currentSec - need;
    const to = currentSec - 1;
    if (to < from) return false;
    let i = 0;
    let runFirstAt: number | undefined;
    for (let sec = from; sec <= to; sec++) {
      while (i < this.lineBuckets.length && this.lineBuckets[i].sec < sec) i++;
      const bucket = this.lineBuckets[i];
      if (!bucket || bucket.sec !== sec || bucket.n < this.stopLinesPerSec) return false;
      if (runFirstAt === undefined) runFirstAt = bucket.firstAt;
    }
    return runFirstAt !== undefined && now - runFirstAt >= this.stopSustainMs;
  }

  private addInterval(input: OverloadInput, now: number): void {
    this.interval.lines += Math.max(0, input.lines);
    this.interval.bytes += Math.max(0, input.bytes);
    if (this.interval.firstAt === undefined) this.interval.firstAt = now;
    this.interval.lastAt = now;
    if (input.cursorStart !== undefined) {
      this.interval.cursorStart = this.interval.cursorStart === undefined
        ? input.cursorStart
        : Math.min(this.interval.cursorStart, input.cursorStart);
    }
    if (input.cursorEnd !== undefined) {
      this.interval.cursorEnd = this.interval.cursorEnd === undefined
        ? input.cursorEnd
        : Math.max(this.interval.cursorEnd, input.cursorEnd);
    }
    this.addSignatures(input.text ?? "", input.lines);
  }

  private addSignatures(text: string, lines: number): void {
    const parts = text.split("\n").filter((line) => line.length > 0);
    if (parts.length === 0) return;
    // Compact form: one line with no newlines uses `lines` as frequency.
    // Empty lines never add signature frequency; they still count in interval lines/bytes.
    if (!text.includes("\n")) {
      this.addSignature(parts[0], Math.max(1, lines));
      return;
    }
    for (const part of parts) this.addSignature(part, 1);
  }

  private addSignature(line: string, count: number): void {
    const key = normalizeSignature(line);
    if (!key) return;
    const existing = this.signatures.get(key);
    if (existing) {
      existing.count += count;
      existing.extra = ++this.sigSeq;
      return;
    }
    while (this.signatures.size >= this.maxSignatures) this.evictLeastFrequent();
    this.signatures.set(key, { count, extra: ++this.sigSeq });
  }

  private evictLeastFrequent(): void {
    let worst: string | undefined;
    let worstCount = Infinity;
    let worstSeq = Infinity;
    for (const [key, val] of this.signatures) {
      if (val.count < worstCount || (val.count === worstCount && val.extra < worstSeq)) {
        worst = key;
        worstCount = val.count;
        worstSeq = val.extra;
      }
    }
    if (worst !== undefined) this.signatures.delete(worst);
  }

  private armCadence(delayMs: number): void {
    this.clearCadence();
    this.cadenceTimer = this.clock.setTimeout(() => this.onCadenceDue(), delayMs);
    this.clock.unref?.(this.cadenceTimer);
  }

  private armExit(delayMs: number): void {
    this.clearExit();
    this.exitTimer = this.clock.setTimeout(() => {
      this.exitTimer = null;
      if (this.disposed || !this.summaryMode) return;
      const now = this.clock.now();
      const busy = this.lineRateHigh(now) || this.saturation.isSaturated(now);
      if (busy) {
        this.belowSince = undefined;
        return;
      }
      this.leaveSummary();
    }, delayMs);
    this.clock.unref?.(this.exitTimer);
  }

  private clearCadence(): void {
    if (this.cadenceTimer !== null) {
      this.clock.clearTimeout(this.cadenceTimer);
      this.cadenceTimer = null;
    }
  }

  private clearExit(): void {
    if (this.exitTimer !== null) {
      this.clock.clearTimeout(this.exitTimer);
      this.exitTimer = null;
    }
  }

  private armWatch(): void {
    this.clearWatch();
    this.watchTimer = this.clock.setInterval(() => {
      if (this.disposed || !this.summaryMode) return;
      this.evaluateExit(this.clock.now());
    }, 1000);
    this.clock.unref?.(this.watchTimer);
  }

  private clearWatch(): void {
    if (this.watchTimer !== null) {
      this.clock.clearInterval(this.watchTimer);
      this.watchTimer = null;
    }
  }
}

function emptyInterval(): {
  lines: number;
  bytes: number;
  firstAt?: number;
  lastAt?: number;
  cursorStart?: number;
  cursorEnd?: number;
} {
  return { lines: 0, bytes: 0 };
}

function formatOverloadSummary(
  interval: { lines: number; bytes: number; firstAt?: number; lastAt?: number; cursorStart?: number; cursorEnd?: number },
  signatures: Map<string, { count: number; extra: number }>,
  maxChars: number,
  newSignatures: number,
): string {
  const top = [...signatures.entries()].sort((a, b) => b[1].count - a[1].count || a[1].extra - b[1].extra).slice(0, 3);
  const other = Math.max(0, signatures.size - top.length);
  const header = [
    "[Monitor overload summary]",
    `lines=${interval.lines} bytes=${interval.bytes} from=${interval.firstAt ?? "-"} to=${interval.lastAt ?? "-"} cursor=${interval.cursorStart ?? "-"}-${interval.cursorEnd ?? "-"}`,
    `raw via task_output cursor=${interval.cursorStart ?? "-"}-${interval.cursorEnd ?? "-"}`,
  ];
  const rows = top.map(([sig, val]) => `${sig} x${val.count}`);
  if (other > 0) rows.push(`+${other} other signatures`);
  rows.push(`${newSignatures} new since previous`);
  let text = [...header, ...rows].join("\n");
  if (text.length <= maxChars) return text;
  text = header.join("\n");
  for (const row of rows) {
    const next = `${text}\n${row}`;
    if (next.length > maxChars) break;
    text = next;
  }
  if (text.length > maxChars) text = text.slice(0, Math.max(0, maxChars));
  return text;
}
