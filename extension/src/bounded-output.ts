/**
 * Bounded collector for a streaming command's output.
 *
 * Keeps only a raw byte tail in memory and, once the output outgrows the
 * display limits, streams the full output to a temp file asynchronously.
 * Memory stays bounded and no step materializes the whole output as one
 * buffer or string on the event loop.
 */
import { randomUUID } from "node:crypto";
import { createWriteStream, unlinkSync, type WriteStream } from "node:fs";
import type { Readable } from "node:stream";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Spill files created this process; unlinked on session shutdown. */
const sessionSpillFiles = new Set<string>();

/** Forget and delete every overflow file this session wrote. */
export function cleanupSessionSpillFiles(): void {
  for (const path of sessionSpillFiles) {
    try {
      unlinkSync(path);
    } catch {
      // already gone, or never created
    }
  }
  sessionSpillFiles.clear();
}

export interface BoundedOutputOptions {
  /** Spill to a temp file once total bytes exceed this. */
  maxBytes: number;
  /** Spill to a temp file once total lines exceed this. */
  maxLines: number;
  /** Raw bytes of tail to keep in memory. */
  tailBytes: number;
  /** Temp file name prefix. */
  tempFilePrefix?: string;
}

export interface BoundedOutputResult {
  /** Decoded tail (the whole output when nothing was dropped). */
  text: string;
  /** True when `text` is missing a head that was dropped from memory. */
  headDropped: boolean;
  totalBytes: number;
  /** Line count as `text.split("\n").length` would give for the full output. */
  totalLines: number;
  /** Full output, present once the output exceeded the limits. */
  fullOutputPath?: string;
}

export class BoundedOutput {
  private chunks: Buffer[] = [];
  private bufferedBytes = 0;
  private headDropped = false;
  private totalBytes = 0;
  private newlines = 0;
  private stream: WriteStream | null = null;
  private streamFailed = false;
  private path: string | undefined;
  private sources: Readable[] = [];
  private waitingForDrain = false;

  constructor(private readonly opts: BoundedOutputOptions) {}

  /**
   * Collect from these streams, pausing them while the spill file drains so
   * output faster than the disk cannot pile up in the write stream's buffer.
   */
  attach(...streams: Array<Readable | null | undefined>): void {
    for (const stream of streams) {
      if (!stream) continue;
      this.sources.push(stream);
      stream.on("data", (d: Buffer) => this.append(d));
    }
  }

  append(data: Buffer): void {
    if (data.length === 0) return;
    this.totalBytes += data.length;
    for (let i = data.indexOf(10); i !== -1; i = data.indexOf(10, i + 1)) this.newlines++;
    if (this.stream === null && !this.streamFailed && this.exceedsLimits()) this.openSpill();
    if (this.stream !== null && !this.stream.write(data)) this.applyBackpressure(this.stream);
    this.chunks.push(data);
    this.bufferedBytes += data.length;
    // Trim lazily (at 2x) so trimming cost is amortized over many chunks.
    if (this.bufferedBytes > this.opts.tailBytes * 2) this.trim();
  }

  async finish(): Promise<BoundedOutputResult> {
    // Sources have ended; drop any pause left from the last write.
    if (this.waitingForDrain) {
      this.waitingForDrain = false;
      for (const source of this.sources) source.resume();
    }
    this.trim();
    let raw = this.chunks.length === 1 ? this.chunks[0] : Buffer.concat(this.chunks);
    if (this.headDropped) raw = skipPartialUtf8(raw);
    const text = raw.toString("utf8");
    let fullOutputPath: string | undefined;
    if (this.stream !== null) {
      const stream = this.stream;
      this.stream = null;
      const ok = await new Promise<boolean>((resolve) => {
        stream.once("error", () => resolve(false));
        stream.end(() => resolve(true));
      });
      if (ok && !this.streamFailed) fullOutputPath = this.path;
    }
    return {
      text,
      headDropped: this.headDropped,
      totalBytes: this.totalBytes,
      totalLines: this.newlines + 1,
      fullOutputPath,
    };
  }

  private applyBackpressure(stream: WriteStream): void {
    if (this.waitingForDrain || this.sources.length === 0) return;
    this.waitingForDrain = true;
    for (const source of this.sources) source.pause();
    const release = () => {
      stream.off("drain", release);
      stream.off("close", release);
      this.waitingForDrain = false;
      for (const source of this.sources) source.resume();
    };
    stream.on("drain", release);
    stream.on("close", release);
  }

  private exceedsLimits(): boolean {
    return this.totalBytes > this.opts.maxBytes || this.newlines + 1 > this.opts.maxLines;
  }

  private openSpill(): void {
    this.path = join(tmpdir(), `${this.opts.tempFilePrefix ?? "pi-famulus-bash"}-${randomUUID()}.log`);
    sessionSpillFiles.add(this.path);
    const stream = createWriteStream(this.path);
    stream.on("error", () => {
      this.streamFailed = true;
      // Stop writing to a failed spill; keep collecting the in-memory tail.
      // autoDestroy then emits "close", which releases paused sources.
      if (this.stream === stream) this.stream = null;
    });
    // Everything seen so far is still in memory: the head is only dropped
    // after the spill file exists.
    this.stream = stream;
    let drained = true;
    for (const chunk of this.chunks) drained = stream.write(chunk);
    if (!drained) this.applyBackpressure(stream);
  }

  private trim(): void {
    const cap = this.opts.tailBytes;
    if (this.bufferedBytes <= cap) return;
    // Spill before dropping anything so the full output survives.
    if (this.stream === null && !this.streamFailed) this.openSpill();
    let excess = this.bufferedBytes - cap;
    while (excess > 0 && this.chunks.length > 0) {
      const first = this.chunks[0];
      if (first.length <= excess) {
        this.chunks.shift();
        excess -= first.length;
        this.bufferedBytes -= first.length;
      } else {
        this.chunks[0] = first.subarray(excess);
        this.bufferedBytes -= excess;
        excess = 0;
      }
    }
    this.headDropped = true;
  }
}

/** Drop leading UTF-8 continuation bytes left by a byte-offset cut. */
function skipPartialUtf8(buf: Buffer): Buffer {
  let i = 0;
  while (i < buf.length && i < 4 && (buf[i] & 0xc0) === 0x80) i++;
  return i === 0 ? buf : buf.subarray(i);
}
