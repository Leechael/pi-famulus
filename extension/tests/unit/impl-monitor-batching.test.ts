import { beforeEach, describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import {
  LineBatcher,
  RateLimiter,
  SaturationWindow,
  OverloadController,
  LegacyLineAssembler,
  LEGACY_PARTIAL_MAX_CHARS,
  normalizeSignature,
  OVERLOAD_WINDOW_MS,
  OVERLOAD_SUMMARY_LINES_PER_SEC,
  OVERLOAD_STOP_LINES_PER_SEC,
  OVERLOAD_STOP_SUSTAIN_MS,
  OVERLOAD_EXIT_QUIET_MS,
  SUMMARY_MAX_CHARS,
  SUMMARY_MAX_SIGNATURES,
} from "../../src/monitor-batching";

describe("LineBatcher", () => {
  let clock: ManualClock;
  beforeEach(() => {
    clock = new ManualClock();
  });

  function collect(opts: Partial<ConstructorParameters<typeof LineBatcher>[0]> = {}) {
    const batches: string[] = [];
    const batcher = new LineBatcher({ onFlush: (text) => batches.push(text), clock, ...opts });
    return { batches, batcher };
  }

  it("coalesces pushes within the flush window into one batch", () => {
    const { batches, batcher } = collect();
    batcher.push("one\n");
    batcher.push("two\n");
    batcher.push("three\n");
    expect(batches).toEqual([]);
    clock.advanceBy(200);
    expect(batches).toEqual(["one\ntwo\nthree"]);
    batcher.dispose();
  });

  it("emits separate batches when pushes are further apart than the window", () => {
    const { batches, batcher } = collect();
    batcher.push("one\n");
    clock.advanceBy(200);
    batcher.push("two\n");
    clock.advanceBy(200);
    expect(batches).toEqual(["one", "two"]);
    batcher.dispose();
  });

  it("concatenates chunks before splitting lines (no invented newlines)", () => {
    const { batches, batcher } = collect();
    batcher.push("hel");
    batcher.push("lo\nworld\n");
    clock.advanceBy(200);
    expect(batches).toEqual(["hello\nworld"]);
    batcher.dispose();
  });

  it("drains an unterminated trailing line at window end (no data loss)", () => {
    const { batches, batcher } = collect();
    batcher.push("done\ntail-without-newline");
    clock.advanceBy(200);
    expect(batches).toEqual(["done\ntail-without-newline"]);
    batcher.dispose();
  });

  it("flush() emits the buffer immediately without waiting for the window", () => {
    const { batches, batcher } = collect();
    batcher.push("abc\n");
    batcher.flush();
    expect(batches).toEqual(["abc"]);
    // The pending timer must not re-deliver the same content.
    clock.advanceBy(1000);
    expect(batches).toEqual(["abc"]);
    batcher.dispose();
  });

  it("caps individual lines at maxLineChars", () => {
    const { batches, batcher } = collect({ maxLineChars: 5 });
    batcher.push("1234567890\n");
    clock.advanceBy(200);
    expect(batches).toEqual(["12…90"]);
    batcher.dispose();
  });

  it("does not count generated ellipses as omitted source characters", () => {
    const infos: { omittedChars: number }[] = [];
    const batcher = new LineBatcher({
      maxLineChars: 5,
      maxBatchChars: 10_000,
      clock,
      onFlush: (_text, info) => infos.push(info),
    });
    for (let i = 0; i < 10; i++) batcher.push("1234567890\n");
    clock.advanceBy(200);
    // Each 10-char line keeps 4 source characters and inserts one ellipsis.
    // Omission is 6 source chars/line, not 7.
    expect(infos[0].omittedChars).toBe(60);
    batcher.dispose();
  });

  it("caps a batch at maxBatchChars keeping the tail", () => {
    const { batches, batcher } = collect({ maxBatchChars: 10 });
    batcher.push("aaaa\nbbbb\ncccc\n");
    clock.advanceBy(200);
    expect(batches).toHaveLength(1);
    expect(batches[0].length).toBe(10);
    expect(batches[0].startsWith("aaaa")).toBe(true);
    expect(batches[0]).toContain("…");
    expect(batches[0].endsWith("cccc")).toBe(true);
    batcher.dispose();
  });

  it("does not emit empty batches", () => {
    const { batches, batcher } = collect();
    batcher.push("");
    batcher.flush();
    clock.advanceBy(1000);
    expect(batches).toEqual([]);
    batcher.dispose();
  });

  it("stops emitting after dispose", () => {
    const { batches, batcher } = collect();
    batcher.push("one\n");
    batcher.dispose();
    clock.advanceBy(1000);
    batcher.push("two\n");
    batcher.flush();
    expect(batches).toEqual([]);
  });

  it("honors a custom flush window", () => {
    const { batches, batcher } = collect({ flushMs: 50 });
    batcher.push("x\n");
    clock.advanceBy(49);
    expect(batches).toEqual([]);
    clock.advanceBy(1);
    expect(batches).toEqual(["x"]);
    batcher.dispose();
  });
});

describe("SaturationWindow", () => {
  it("is saturated only after a full window with at least half of batches dropped", () => {
    const window = new SaturationWindow({ windowMs: 30_000, dropRatio: 0.5, minimumBatches: 10 });
    for (let i = 0; i < 10; i++) window.record(i % 2 === 0, i * 2_000);
    expect(window.isSaturated(29_999)).toBe(false);
    expect(window.isSaturated(30_000)).toBe(true);
  });

  it("does not trip when drops fall below half of the rolling window", () => {
    const window = new SaturationWindow({ windowMs: 30_000, dropRatio: 0.5, minimumBatches: 10 });
    for (let i = 0; i < 10; i++) window.record(i < 4, i * 2_000);
    expect(window.isSaturated(30_000)).toBe(false);
  });

  it("expires old samples so a past burst cannot keep saturation latched", () => {
    const window = new SaturationWindow({ windowMs: 30_000, dropRatio: 0.5, minimumBatches: 10 });
    for (let i = 0; i < 10; i++) window.record(true, i * 1_000);
    expect(window.isSaturated(30_000)).toBe(true);
    for (let i = 0; i < 10; i++) window.record(false, 31_000 + i * 1_000);
    expect(window.isSaturated(60_000)).toBe(false);
  });

  it("enters after a full window of 201ms 100% drops even when the earliest retained sample is not exactly 30s old", () => {
    const window = new SaturationWindow({ windowMs: 30_000, dropRatio: 0.5, minimumBatches: 10 });
    for (let t = 0; t <= 80_000; t += 201) window.record(true, t);
    expect(window.isSaturated(80_000)).toBe(true);
    expect(window.bucketCountForTest()).toBeLessThanOrEqual(32);
  });

  it("hard-bounds drop state with per-second buckets under 100k same-time batches", () => {
    const window = new SaturationWindow({ windowMs: 30_000, dropRatio: 0.5, minimumBatches: 10 });
    for (let i = 0; i < 100_000; i++) window.record(true, 1_000);
    expect(window.bucketCountForTest()).toBeLessThanOrEqual(2);
    expect(window.isSaturated(31_000)).toBe(true);
  });
});

describe("RateLimiter", () => {
  let clock: ManualClock;
  beforeEach(() => {
    clock = new ManualClock();
  });

  it("allows up to capacity tokens, then refuses", () => {
    const limiter = new RateLimiter({ capacity: 3, refillIntervalMs: 1000, refillAmount: 1, clock });
    expect(limiter.tryConsume()).toBe(true);
    expect(limiter.tryConsume()).toBe(true);
    expect(limiter.tryConsume()).toBe(true);
    expect(limiter.tryConsume()).toBe(false);
    limiter.dispose();
  });

  it("refills tokens at the configured interval", () => {
    const limiter = new RateLimiter({ capacity: 2, refillIntervalMs: 2000, refillAmount: 1, clock });
    expect(limiter.tryConsume(2)).toBe(true);
    expect(limiter.tryConsume()).toBe(false);
    clock.advanceBy(2000);
    expect(limiter.tryConsume()).toBe(true);
    expect(limiter.tryConsume()).toBe(false);
    clock.advanceBy(4000);
    // Refill never exceeds capacity.
    expect(limiter.tryConsume(2)).toBe(true);
    expect(limiter.tryConsume()).toBe(false);
    limiter.dispose();
  });

  it("supports consuming multiple tokens at once", () => {
    const limiter = new RateLimiter({ capacity: 5, clock });
    expect(limiter.tryConsume(4)).toBe(true);
    expect(limiter.tryConsume(2)).toBe(false);
    expect(limiter.tryConsume(1)).toBe(true);
    limiter.dispose();
  });

  it("uses the documented defaults (capacity 10, +1 per 2s)", () => {
    const limiter = new RateLimiter({ clock });
    for (let i = 0; i < 10; i++) expect(limiter.tryConsume()).toBe(true);
    expect(limiter.tryConsume()).toBe(false);
    clock.advanceBy(2000);
    expect(limiter.tryConsume()).toBe(true);
    limiter.dispose();
  });

  it("refuses consumption after dispose", () => {
    const limiter = new RateLimiter({ capacity: 1, clock });
    limiter.dispose();
    expect(limiter.tryConsume()).toBe(false);
  });
});

describe("normalizeSignature", () => {
  it("collapses request-id-only variants to the same signature", () => {
    const a = normalizeSignature("error request-id=550e8400-e29b-41d4-a716-446655440000 timeout");
    const b = normalizeSignature("error request-id=123e4567-e89b-12d3-a456-426614174000 timeout");
    expect(a).toBe(b);
    expect(a).toContain("error");
    expect(a).not.toMatch(/[0-9a-f]{8}-[0-9a-f]{4}/i);
  });

  it("keeps HTTP 500 distinct from HTTP 502", () => {
    expect(normalizeSignature("upstream HTTP 500 from app")).not.toBe(
      normalizeSignature("upstream HTTP 502 from app"),
    );
    expect(normalizeSignature("upstream HTTP 500 from app")).toContain("500");
    expect(normalizeSignature("upstream HTTP 502 from app")).toContain("502");
  });

  it("strips hex>=8, ISO/common timestamps, IPv4/IPv6, and digit runs>=4, keeping 1-3 digit runs", () => {
    const sig = normalizeSignature(
      "2026-10-29T12:34:56.789Z host 10.0.0.1 deadbeefcafebabe port 22 code 1234",
    );
    expect(sig).not.toMatch(/2026-10-29/);
    expect(sig).not.toContain("10.0.0.1");
    expect(sig).not.toMatch(/deadbeef/i);
    expect(sig).not.toContain("1234");
    expect(sig).toContain("22");
    expect(normalizeSignature("code 999")).toContain("999");
  });

  it("strips compressed and full IPv6, including bracketed-with-port, without collapsing HTTP 500/502", () => {
    const cases = [
      "peer ::1 refused",
      "peer 2001:db8::1 refused",
      "peer https://example.com/x/prism refused",
      "peer [2001:db8::1]:443 refused",
      "peer [::1]:8080 refused",
      "peer fe80::1 refused",
      "peer 127.0.0.1:22 refused",
    ];
    for (const line of cases) {
      const sig = normalizeSignature(line);
      expect(sig, line).not.toMatch(/::/);
      expect(sig, line).not.toMatch(/2001:db8/i);
      expect(sig, line).not.toMatch(/0db8/i);
      expect(sig, line).not.toContain("127.0.0.1");
      expect(sig, line).toContain("peer");
      expect(sig, line).toContain("refused");
    }
    expect(normalizeSignature("upstream HTTP 500 from app")).toContain("500");
    expect(normalizeSignature("upstream HTTP 502 from app")).toContain("502");
    expect(normalizeSignature("note a:b:c kept")).toContain("a:b:c");
  });

  it("truncates to 120 characters", () => {
    expect(normalizeSignature(`prefix ${"a".repeat(200)}`).length).toBeLessThanOrEqual(120);
  });

  it("does not over-strip common HTTP status, URL paths, or Class::method", () => {
    const http = normalizeSignature("GET /api/v2/users HTTP 404");
    expect(http).toContain("GET");
    expect(http).toContain("/api/v");
    expect(http).toContain("404");
    expect(normalizeSignature("HTTP 200 ok")).toContain("200");
    expect(normalizeSignature("HTTP 301 redirect")).toContain("301");
    expect(normalizeSignature("Foo::bar failed")).toContain("Foo::bar");
    expect(normalizeSignature("UserController::show missing")).toContain("UserController::show");
  });

  it("strips quoted compressed IPv6 without residue while keeping HTTP 500", () => {
    for (const line of [
      'peer="2001:db8::1" HTTP 500',
      "peer='2001:db8::1' HTTP 500",
    ]) {
      const sig = normalizeSignature(line);
      expect(sig, line).not.toMatch(/2001:db8/i);
      expect(sig, line).not.toMatch(/::/);
      expect(sig, line).toContain("HTTP");
      expect(sig, line).toContain("500");
      expect(sig, line).toContain("peer");
    }
  });
});

describe("OverloadController", () => {
  let clock: ManualClock;
  beforeEach(() => {
    clock = new ManualClock();
  });

  function collect(opts: ConstructorParameters<typeof OverloadController>[0] = {}) {
    const summaries: { text: string; output?: { cursorStart?: number; cursorEnd?: number; firstObservedAt?: number; lastObservedAt?: number } }[] = [];
    const enters: string[] = [];
    const exits: number[] = [];
    const controller = new OverloadController({
      clock,
      onSummary: (text, output) => summaries.push({ text, output }),
      onEnter: (reason) => enters.push(reason),
      onExit: () => exits.push(clock.now()),
      ...opts,
    });
    return { controller, summaries, enters, exits };
  }

  it("enters summary when rolling 30s input average exceeds 5 lines/sec (draft, owner-unconfirmed)", () => {
    const { controller } = collect();
    expect(OVERLOAD_SUMMARY_LINES_PER_SEC).toBe(5);
    expect(OVERLOAD_WINDOW_MS).toBe(30_000);
    for (let s = 0; s < 30; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 6, bytes: 60, text: "HTTP 500 boom", cursorStart: s * 60, cursorEnd: s * 60 + 60 });
      expect(controller.inSummary()).toBe(false);
    }
    clock.advanceTo(30_000);
    controller.recordInput({ lines: 6, bytes: 60, text: "HTTP 500 boom", cursorStart: 1800, cursorEnd: 1860 });
    expect(controller.inSummary()).toBe(true);
    expect(controller.shouldStop()).toBe(false);
    controller.dispose();
  });

  it("enters summary on full-window drop ratio >= half even when line rate stays below 5", () => {
    const { controller } = collect();
    for (let i = 0; i < 10; i++) {
      clock.advanceTo(i * 3_000);
      controller.recordInput({ lines: 1, bytes: 4, text: "tick" });
      controller.recordBatch(i % 2 === 0);
    }
    expect(controller.inSummary()).toBe(false);
    clock.advanceTo(30_000);
    controller.recordInput({ lines: 1, bytes: 4, text: "tick" });
    controller.recordBatch(true);
    expect(controller.inSummary()).toBe(true);
    expect(controller.shouldStop()).toBe(false);
    controller.dispose();
  });

  it("starts interval stats at summary entry: excludes pre-entry raw lines and includes the line-rate trigger", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
    });
    controller.recordInput({ lines: 3, bytes: 30, text: "pre-entry HTTP 500", cursorStart: 0, cursorEnd: 30 });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 6, bytes: 60, text: "HTTP 502 trigger", cursorStart: 100, cursorEnd: 160 });
    expect(controller.inSummary()).toBe(true);
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(summaries[0].text).toMatch(/lines=6/);
    expect(summaries[0].text).not.toMatch(/lines=9/);
    expect(summaries[0].text).toContain("HTTP 502");
    expect(summaries[0].text).not.toContain("HTTP 500");
    expect(summaries[0].output).toMatchObject({ cursorStart: 100, cursorEnd: 160 });
    controller.dispose();
  });

  it("starts drop-ratio summary stats empty, excluding pre-entry raw lines", () => {
    const { controller, summaries } = collect({ exitQuietMs: 3_600_000 });
    for (let i = 0; i < 10; i++) {
      clock.advanceTo(i * 3_000);
      controller.recordInput({ lines: 1, bytes: 4, text: "pre-drop tick" });
      controller.recordBatch(i % 2 === 0);
    }
    clock.advanceTo(30_000);
    controller.recordInput({ lines: 1, bytes: 4, text: "pre-drop tick" });
    controller.recordBatch(true);
    expect(controller.inSummary()).toBe(true);
    controller.recordInput({ lines: 2, bytes: 8, text: "post-entry HTTP 502", cursorStart: 50, cursorEnd: 58 });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(summaries[0].text).toMatch(/lines=2/);
    expect(summaries[0].text).toContain("HTTP 502");
    expect(summaries[0].text).not.toContain("tick");
    expect(summaries[0].output).toMatchObject({ cursorStart: 50, cursorEnd: 58 });
    controller.dispose();
  });

  it("emits summaries on 30s/60s/120s/300s cadence with since-previous totals, cursor, and top-3 signatures", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
      cadenceMs: [30_000, 60_000, 120_000, 300_000],
    });
    const body = `${"HTTP 500 a\n".repeat(100)}${"HTTP 502 b\n".repeat(40)}${"reset c\n".repeat(16)}`;
    controller.recordInput({ lines: 3, bytes: 30, text: "HTTP 500 a" });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 156, bytes: 1560, text: body, cursorStart: 10, cursorEnd: 1570 });
    expect(controller.inSummary()).toBe(true);
    expect(summaries).toEqual([]);
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(summaries[0].text.length).toBeLessThanOrEqual(SUMMARY_MAX_CHARS);
    expect(summaries[0].text).toMatch(/lines=156/);
    expect(summaries[0].text).toMatch(/bytes=1560/);
    expect(summaries[0].output).toMatchObject({ cursorStart: 10, cursorEnd: 1570 });
    expect(summaries[0].text.indexOf("HTTP 500")).toBeLessThan(summaries[0].text.indexOf("HTTP 502"));
    expect(summaries[0].text).toMatch(/raw via task_output cursor=10-1570/);
    expect(summaries[0].text).toMatch(/new since previous/);
    controller.recordInput({ lines: 3, bytes: 9, text: "HTTP 500 a", cursorStart: 2000, cursorEnd: 2009 });
    clock.advanceBy(60_000);
    expect(summaries).toHaveLength(2);
    expect(summaries[1].text).toMatch(/lines=3/);
    expect(summaries[1].output).toMatchObject({ cursorStart: 2000, cursorEnd: 2009 });
    controller.recordInput({ lines: 1, bytes: 1, text: "z", cursorStart: 3000, cursorEnd: 3001 });
    clock.advanceBy(120_000);
    expect(summaries).toHaveLength(3);
    controller.recordInput({ lines: 1, bytes: 1, text: "z", cursorStart: 4000, cursorEnd: 4001 });
    clock.advanceBy(300_000);
    expect(summaries).toHaveLength(4);
    controller.recordInput({ lines: 1, bytes: 1, text: "z", cursorStart: 5000, cursorEnd: 5001 });
    clock.advanceBy(300_000);
    expect(summaries).toHaveLength(5);
    controller.dispose();
  });

  it("sends nothing and does not advance cadence when the interval has 0 lines", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
    });
    controller.recordInput({ lines: 3, bytes: 3, text: "HTTP 500" });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 156, bytes: 156, text: "HTTP 500" });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    clock.advanceBy(60_000);
    expect(summaries).toHaveLength(1);
    controller.recordInput({ lines: 2, bytes: 2, text: "HTTP 500" });
    clock.advanceBy(60_000);
    expect(summaries).toHaveLength(2);
    expect(summaries[1].text).toMatch(/lines=2/);
    controller.dispose();
  });

  it("exits summary after both triggers stay below threshold for 60s and resets cadence", () => {
    const { controller, summaries } = collect();
    expect(OVERLOAD_EXIT_QUIET_MS).toBe(60_000);
    for (let s = 0; s <= 30; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 6, bytes: 6, text: "HTTP 500" });
    }
    expect(controller.inSummary()).toBe(true);
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    clock.advanceBy(OVERLOAD_WINDOW_MS + OVERLOAD_EXIT_QUIET_MS);
    expect(controller.inSummary()).toBe(false);
    for (let s = 0; s <= 30; s++) {
      clock.advanceBy(1_000);
      controller.recordInput({ lines: 6, bytes: 6, text: "HTTP 500" });
    }
    expect(controller.inSummary()).toBe(true);
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(2);
    controller.dispose();
  });

  it("one nonempty line plus 100 empty lines counts signature x1, not x101", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
    });
    controller.recordInput({ lines: 3, bytes: 3, text: "pre" });
    clock.advanceBy(1_000);
    const text = `HTTP 500${"\n".repeat(100)}`;
    controller.recordInput({
      lines: 101,
      bytes: Buffer.byteLength(text),
      text,
      cursorStart: 0,
      cursorEnd: Buffer.byteLength(text),
    });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(summaries[0].text).toMatch(/lines=101/);
    expect(summaries[0].text).toMatch(/bytes=108/);
    expect(summaries[0].text).toMatch(/HTTP 500 x1/);
    expect(summaries[0].text).not.toMatch(/HTTP 500 x101/);
    expect(summaries[0].text).not.toMatch(/^x\d+$/m);
    controller.dispose();
  });

  it("does not auto-stop on drop ratio; only sustained 1000 input lines/sec for 60s stops", () => {
    const { controller } = collect();
    expect(OVERLOAD_STOP_LINES_PER_SEC).toBe(1000);
    expect(OVERLOAD_STOP_SUSTAIN_MS).toBe(60_000);
    for (let i = 0; i < 20; i++) {
      clock.advanceTo(i * 2_000);
      controller.recordBatch(true);
    }
    expect(controller.inSummary()).toBe(true);
    expect(controller.shouldStop()).toBe(false);
    for (let s = 0; s < 60; s++) {
      clock.advanceTo(40_000 + s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
      expect(controller.shouldStop()).toBe(false);
    }
    clock.advanceTo(40_000 + 60 * 1000);
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("does not stop on a one-second burst even at 60s later", () => {
    const { controller } = collect();
    controller.recordInput({ lines: 100_000, bytes: 100_000, text: "flood" });
    clock.advanceBy(60_000);
    expect(controller.shouldStop()).toBe(false);
    controller.dispose();
  });

  it("caps each summary at 1500 characters", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
    });
    const lines = Array.from({ length: 200 }, (_, i) => `unique signature number ${i} ${"x".repeat(80)}`).join("\n");
    controller.recordInput({ lines: 3, bytes: 30, text: "HTTP 500" });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 200, bytes: 20_000, text: lines });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(summaries[0].text.length).toBeLessThanOrEqual(1500);
    controller.dispose();
  });

  it("evicts the least-frequent signature after 64 and stays bounded under massive input", () => {
    const { controller } = collect({ exitQuietMs: 3_600_000 });
    for (let i = 0; i < 10; i++) {
      clock.advanceTo(i * 3_000);
      controller.recordInput({ lines: 1, bytes: 4, text: "tick" });
      controller.recordBatch(i % 2 === 0);
    }
    clock.advanceTo(30_000);
    controller.recordBatch(true);
    expect(controller.inSummary()).toBe(true);
    for (let i = 0; i < SUMMARY_MAX_SIGNATURES; i++) {
      controller.recordInput({ lines: 1, bytes: 10, text: `sig-${i}-aaa` });
    }
    controller.recordInput({ lines: 5, bytes: 50, text: "sig-0-aaa\n".repeat(5) });
    controller.recordInput({ lines: 1, bytes: 10, text: "sig-new-zzz" });
    const snap = controller.signatureStateForTest();
    expect(snap.size).toBe(SUMMARY_MAX_SIGNATURES);
    expect([...snap.keys()].some((k) => k.includes("sig-new"))).toBe(true);
    expect([...snap.keys()].some((k) => k.includes("sig-0"))).toBe(true);
    for (let i = 0; i < 10_000; i++) {
      controller.recordInput({ lines: 1, bytes: 8, text: `flood-${i}-yyyy` });
    }
    expect(controller.signatureStateForTest().size).toBeLessThanOrEqual(SUMMARY_MAX_SIGNATURES);
    expect(controller.bucketCountForTest()).toBeLessThanOrEqual(61);
    controller.dispose();
  });

  it("exactly 5 lines/s at t=30s does not enter; threshold is >5, including nonintegral start", () => {
    const { controller } = collect();
    for (let s = 0; s <= 30; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 5, bytes: 5, text: "tick" });
    }
    expect(controller.inSummary()).toBe(false);
    controller.dispose();
    const clock2 = new ManualClock();
    const c2 = new OverloadController({ clock: clock2 });
    for (let s = 0; s < 31; s++) {
      clock2.advanceTo(999 + s * 1000);
      c2.recordInput({ lines: 5, bytes: 5, text: "tick" });
    }
    expect(c2.inSummary()).toBe(false);
    c2.dispose();
  });

  it("emits exactly one enter notice, suppresses waiting for the first summary at 30s, and one exit notice", () => {
    const { controller, summaries, enters, exits } = collect();
    for (let s = 0; s <= 30; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 6, bytes: 6, text: "HTTP 500" });
    }
    expect(enters).toEqual(["line-rate"]);
    expect(summaries).toEqual([]);
    expect(exits).toEqual([]);
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    expect(enters).toHaveLength(1);
    clock.advanceBy(OVERLOAD_WINDOW_MS + OVERLOAD_EXIT_QUIET_MS);
    expect(exits).toHaveLength(1);
    expect(controller.inSummary()).toBe(false);
    expect(enters).toHaveLength(1);
    controller.dispose();
  });

  it("flushes a nonempty interval before the exit notice without advancing 120/300 cadence", () => {
    const { controller, summaries, exits } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      cadenceMs: [30_000, 60_000, 120_000, 300_000],
    });
    controller.recordInput({ lines: 3, bytes: 3, text: "HTTP 500" });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 6, bytes: 6, text: "HTTP 500" });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(1);
    controller.recordInput({ lines: 4, bytes: 4, text: "HTTP 502", cursorStart: 9, cursorEnd: 13 });
    clock.advanceBy(2_000 + OVERLOAD_EXIT_QUIET_MS);
    expect(controller.inSummary()).toBe(false);
    expect(summaries).toHaveLength(2);
    expect(summaries[1].text).toMatch(/lines=4/);
    expect(summaries[1].text).toContain("HTTP 502");
    expect(exits).toHaveLength(1);
    expect(exits[0]).toBeGreaterThan(0);
    controller.recordInput({ lines: 3, bytes: 3, text: "HTTP 500" });
    clock.advanceBy(1_000);
    controller.recordInput({ lines: 6, bytes: 6, text: "HTTP 500" });
    clock.advanceBy(30_000);
    expect(summaries).toHaveLength(3);
    controller.dispose();
  });

  it("adds +N other signatures and new-since-previous on each summary", () => {
    const { controller, summaries } = collect({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 3_600_000,
    });
    controller.recordInput({ lines: 3, bytes: 3, text: "pre" });
    clock.advanceBy(1_000);
    const body = ["HTTP 500 a", "HTTP 502 b", "reset c", "other d", "other e"].join("\n");
    controller.recordInput({ lines: 5, bytes: 50, text: body, cursorStart: 1, cursorEnd: 51 });
    clock.advanceBy(30_000);
    expect(summaries[0].text).toMatch(/\+2 other signatures/);
    expect(summaries[0].text).toMatch(/5 new since previous/);
    expect(summaries[0].text).toMatch(/raw via task_output cursor=1-51/);
    controller.recordInput({ lines: 2, bytes: 20, text: "HTTP 500 a\nbrand-new z", cursorStart: 60, cursorEnd: 80 });
    clock.advanceBy(60_000);
    expect(summaries[1].text).toMatch(/1 new since previous/);
    controller.dispose();
  });

  it("stop duration uses current continuous high-rate start, not firstInputAt", () => {
    const { controller } = collect();
    controller.recordInput({ lines: 1, bytes: 1, text: "early" });
    for (let s = 0; s < 60; s++) {
      clock.advanceTo(100_999 + s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
    }
    clock.advanceTo(160_000);
    expect(controller.shouldStop()).toBe(false);
    clock.advanceTo(100_999 + 60_000);
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("resets auto-stop continuity on a below-threshold second", () => {
    const { controller } = collect();
    for (let s = 0; s < 40; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
    }
    clock.advanceTo(40_000);
    controller.recordInput({ lines: 1, bytes: 1, text: "low" });
    for (let s = 41; s <= 100; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
      expect(controller.shouldStop(), `t=${s * 1000}`).toBe(false);
    }
    clock.advanceTo(101_000);
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("resets auto-stop continuity on a gap second", () => {
    const { controller } = collect();
    for (let s = 0; s < 40; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
    }
    for (let s = 41; s <= 100; s++) {
      clock.advanceTo(s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
      expect(controller.shouldStop(), `t=${s * 1000}`).toBe(false);
    }
    clock.advanceTo(101_000);
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("non-integral high-rate start with preexisting history cannot stop before 60s elapsed", () => {
    const { controller } = collect();
    clock.advanceTo(10);
    controller.recordInput({ lines: 2, bytes: 2, text: "pre" });
    clock.advanceTo(999);
    for (let s = 0; s < 60; s++) {
      clock.advanceTo(999 + s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
      expect(controller.shouldStop(), `t=${999 + s * 1000}`).toBe(false);
    }
    clock.advanceTo(999 + 59_000);
    expect(controller.shouldStop()).toBe(false);
    clock.advanceTo(60_010);
    expect(controller.shouldStop(), "low-rate input earlier in the first second does not start the flood").toBe(false);
    clock.advanceTo(999 + 60_000);
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("first input at 999ms cannot stop at 59s; stop needs 60 completed seconds", () => {
    const { controller } = collect();
    clock.advanceTo(999);
    for (let s = 0; s < 60; s++) {
      clock.advanceTo(999 + s * 1000);
      controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
      expect(controller.shouldStop(), `t=${999 + s * 1000}`).toBe(false);
    }
    clock.advanceTo(999 + 59_000);
    expect(controller.shouldStop()).toBe(false);
    clock.advanceTo(999 + 60_000);
    controller.recordInput({ lines: 1000, bytes: 1000, text: "flood" });
    expect(controller.shouldStop()).toBe(true);
    controller.dispose();
  });

  it("bounds drop buckets independently of batch rate", () => {
    const { controller } = collect();
    for (let i = 0; i < 100_000; i++) controller.recordBatch(true);
    expect(controller.dropBucketCountForTest()).toBeLessThanOrEqual(2);
    controller.dispose();
  });
});

describe("RateLimiter + OverloadController (200ms batch / token bucket)", () => {
  function pump(linesPerSec: number, durationMs: number, distinct: boolean) {
    const clock = new ManualClock();
    const limiter = new RateLimiter({ clock });
    const enters: string[] = [];
    const controller = new OverloadController({
      clock,
      onEnter: (reason) => enters.push(reason),
    });
    const interval = 1000 / linesPerSec;
    const n = Math.floor(durationMs / interval) + 1;
    for (let i = 0; i < n; i++) {
      const t = i * interval;
      clock.advanceTo(t);
      controller.recordInput({
        lines: 1,
        bytes: 8,
        text: distinct ? `line ${i}` : "dup",
      });
      clock.advanceTo(t + 200);
      const accepted = limiter.tryConsume();
      controller.recordBatch(!accepted);
    }
    return { controller, limiter, enters };
  }

  it("steady distinct 2 lines/s for 30s enters via drop-ratio; 1 line/s does not enter in the first 30s", () => {
    const two = pump(2, 30_000, true);
    expect(two.controller.inSummary()).toBe(true);
    expect(two.enters).toEqual(["drop-ratio"]);
    expect(two.controller.shouldStop()).toBe(false);
    two.controller.dispose();
    two.limiter.dispose();
    const one = pump(1, 30_000, true);
    expect(one.controller.inSummary()).toBe(false);
    expect(one.enters).toEqual([]);
    one.controller.dispose();
    one.limiter.dispose();
  });

  it("0.8 distinct batches/s never enters in 5min; 1.2/s skips first 30s then enters; 2/s enters by 30s", () => {
    const slow = pump(0.8, 300_000, true);
    expect(slow.controller.inSummary()).toBe(false);
    expect(slow.enters).toEqual([]);
    slow.controller.dispose();
    slow.limiter.dispose();

    const mid30 = pump(1.2, 30_000, true);
    expect(mid30.controller.inSummary()).toBe(false);
    mid30.controller.dispose();
    mid30.limiter.dispose();
    const mid = pump(1.2, 120_000, true);
    expect(mid.controller.inSummary()).toBe(true);
    expect(mid.enters).toEqual(["drop-ratio"]);
    mid.controller.dispose();
    mid.limiter.dispose();

    const two = pump(2, 30_000, true);
    expect(two.controller.inSummary()).toBe(true);
    expect(two.enters).toEqual(["drop-ratio"]);
    two.controller.dispose();
    two.limiter.dispose();
  });
});

describe("LegacyLineAssembler", () => {
  it("joins HTTP + 500\\n into one 9-byte HTTP 500 line with full cursor", () => {
    const assembler = new LegacyLineAssembler();
    expect(assembler.push("HTTP ", 5, 0, 5)).toBeUndefined();
    expect(assembler.push("500\n", 4, 5, 9)).toEqual({
      lines: 1,
      bytes: 9,
      text: "HTTP 500",
      cursorStart: 0,
      cursorEnd: 9,
    });
  });

  it("bounds retained partials and still counts full bytes on completion", () => {
    const assembler = new LegacyLineAssembler();
    expect(assembler.push("x".repeat(10_000), 10_000, 0, 10_000)).toBeUndefined();
    expect(assembler.retainedCharsForTest()).toBeLessThanOrEqual(LEGACY_PARTIAL_MAX_CHARS);
    const out = assembler.push("\n", 1, 10_000, 10_001);
    expect(out?.lines).toBe(1);
    expect(out?.bytes).toBe(10_001);
    expect(out?.text?.length).toBeLessThanOrEqual(LEGACY_PARTIAL_MAX_CHARS);
  });

  it("does not emit a final partial without a newline", () => {
    const assembler = new LegacyLineAssembler();
    expect(assembler.push("HTTP 500", 8, 0, 8)).toBeUndefined();
    expect(assembler.retainedCharsForTest()).toBeLessThanOrEqual(LEGACY_PARTIAL_MAX_CHARS);
  });

  it("does not join a partial across a cursor discontinuity", () => {
    const assembler = new LegacyLineAssembler();
    expect(assembler.push("HTTP ", 5, 0, 5)).toBeUndefined();
    expect(assembler.push("next\n", 5, 9, 14)).toEqual({
      lines: 1,
      bytes: 5,
      text: "next",
      cursorStart: 9,
      cursorEnd: 14,
    });
  });
});
