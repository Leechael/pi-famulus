import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { formatMonitorEvent } from "../../src/format";
import { NotifyCenter } from "../../src/notify";
import type { FamulusWake } from "../../src/wake";

describe("NotifyCenter monitor batching", () => {
  it("coalesces busy monitor output per monitor and flushes one wake when idle", () => {
    let idle = false;
    const sent: { message: { customType: string; content: string; details?: unknown }; options: unknown }[] = [];
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ message, options }),
      isIdle: () => idle,
      clock: new ManualClock(),
      logEvent: (type, fields) => events.push({ type, fields }),
    });

    center.notifyMonitorEvent("ticker", "mon_1", "tick 1");
    center.notifyMonitorEvent("ticker", "mon_1", "tick 2", 3);
    center.notifyMonitorEvent("other", "mon_2", "ready");
    expect(sent).toHaveLength(0);

    idle = true;
    center.flushMonitorEvents();
    expect(sent).toHaveLength(2);
    const first = sent[0].message.details as FamulusWake;
    expect(first).toMatchObject({
      kind: "monitor",
      id: "mon_1",
      event: "2 events\ntick 1\ntick 2",
      eventCount: 2,
      droppedLines: 3,
    });
    expect(first.kind === "monitor" ? first.event : "").toBe("2 events\ntick 1\ntick 2");
    expect(sent[0].message.customType).toBe("pi-famulus-wake");
    expect(sent[0].message.content).toContain('<pi-famulus-wake kind="monitor"');
    expect(sent[0].message.content).toContain("</pi-famulus-wake>");
    expect(sent[0].options).toEqual({ triggerTurn: true });
    expect(sent[1].message.details).toMatchObject({ kind: "monitor", id: "mon_2", event: "ready" });
    expect(events).toContainEqual({ type: "wake.emit", fields: { kind: "monitor", ids: ["mon_1"], batch: true } });
    expect(events).toContainEqual({ type: "wake.deliver", fields: { kind: "monitor", mode: "trigger" } });
    center.dispose();
  });

  it("bounds a long busy preview with its original head, latest tail, and an exact omission count", () => {
    let idle = false;
    const sent: { details?: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => idle,
      clock: new ManualClock(),
    });
    const chunks = ["START\n" + "a".repeat(6_000), "b".repeat(6_000), "LATEST\nEND"];
    for (const chunk of chunks) center.notifyMonitorEvent("noisy", "mon_1", chunk, 0, { output: { omittedChars: 5, truncated: false } });
    expect(sent).toHaveLength(0);
    idle = true;
    center.settled();
    const wake = sent[0].details as Extract<FamulusWake, { kind: "monitor" }>;
    expect(wake.eventCount).toBe(3);
    expect(wake.event.length).toBeLessThanOrEqual(3_000);
    expect(wake.event).toMatch(/^3 events\nSTART\n/);
    expect(wake.event).toMatch(/LATEST\nEND$/);
    const marker = /\n\[\.\.\. (\d+) characters omitted \.\.\.\]\n/.exec(wake.event);
    expect(marker).not.toBeNull();
    const omitted = Number(marker![1]);
    expect(omitted).toBe(chunks.join("\n").length - (wake.event.length - "3 events\n".length - marker![0].length));
    expect(wake).toMatchObject({ output: { truncated: true, omittedChars: omitted + 15 } });
    center.dispose();
  });

  it("merges pending observation times, byte envelopes, suppression, and upstream omissions", () => {
    let idle = false;
    const sent: { details?: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => idle,
      clock: new ManualClock(),
    });
    center.notifyMonitorEvent("ticker", "mon_1", "first", 2, {
      output: { firstObservedAt: 20, lastObservedAt: 30, cursorStart: 100, cursorEnd: 140, suppressedCount: 2, omittedChars: 5, truncated: true },
    });
    center.notifyMonitorEvent("ticker", "mon_1", "second", 3, {
      output: { firstObservedAt: 10, lastObservedAt: 25, cursorStart: 50, cursorEnd: 120, suppressedCount: 3, omittedChars: 7, truncated: false },
    });
    center.notifyMonitorEvent("ticker", "mon_1", "third", 0, {
      output: { firstObservedAt: undefined, lastObservedAt: 40, cursorStart: undefined, cursorEnd: 200 },
    });
    idle = true;
    center.settled();
    expect(sent[0].details).toMatchObject({
      event: "3 events\nfirst\nsecond\nthird",
      droppedLines: 5,
      output: { firstObservedAt: 10, lastObservedAt: 40, cursorStart: 50, cursorEnd: 200, suppressedCount: 5, omittedChars: 12, truncated: true },
    });
    center.dispose();
  });

  it("calls one stable monitor acknowledgement after delivery, never on busy enqueue", () => {
    let idle = false;
    const sent: { details?: unknown }[] = [];
    const acknowledgedAt: number[] = [];
    const onDelivered = () => acknowledgedAt.push(sent.length);
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => idle,
      clock: new ManualClock(),
    });
    center.notifyMonitorEvent("ticker", "mon_1", "first", 0, { onDelivered });
    center.notifyMonitorEvent("ticker", "mon_1", "second", 0, { onDelivered });
    expect(acknowledgedAt).toEqual([]);
    idle = true;
    center.settled();
    center.flushMonitorEvents();
    expect(acknowledgedAt).toEqual([1]);
    center.notifyMonitorEvent("ticker", "mon_1", "idle", 0, {
      onDelivered,
      output: { firstObservedAt: 10, cursorStart: 0, cursorEnd: 4 },
    });
    expect(acknowledgedAt).toEqual([1, 2]);
    expect(sent[1].details).toMatchObject({ output: { firstObservedAt: 10, cursorStart: 0, cursorEnd: 4 } });
    center.dispose();
  });

  it("rejects changing a pending monitor's callback without losing its original batch", () => {
    let idle = false;
    const sent: { details?: unknown }[] = [];
    let acknowledged = 0;
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => idle,
      clock: new ManualClock(),
    });
    const onDelivered = () => { acknowledged++; };
    center.notifyMonitorEvent("ticker", "mon_1", "original", 2, { onDelivered, output: { cursorEnd: 8 } });
    expect(() => center.notifyMonitorEvent("changed", "mon_1", "rejected", 9, {
      quiet: true, onDelivered: () => { throw new Error("must not be retained"); }, output: { cursorEnd: 100 },
    })).toThrow(/stable.*onDelivered/i);
    expect(sent).toHaveLength(0);
    idle = true;
    center.settled();
    expect(sent).toHaveLength(1);
    expect(sent[0].details).toMatchObject({ description: "ticker", event: "original", eventCount: 1, droppedLines: 2, output: { cursorEnd: 8 } });
    expect(acknowledged).toBe(1);
    center.dispose();
  });

  it("acknowledges quiet output only when its held passive wake is actually appended", () => {
    let idle = false;
    const clock = new ManualClock(1_000);
    const sent: { message: { details?: unknown }; options: unknown }[] = [];
    const acknowledgedAt: number[] = [];
    const onDelivered = () => acknowledgedAt.push(sent.length);
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ message, options }),
      isIdle: () => idle,
      clock,
    });
    center.notifyMonitorEvent("watcher", "mon_1", "before stop", 0, { onDelivered });
    center.notifyMonitorEvent("watcher", "mon_1", "last line", 0, { quiet: true, onDelivered });
    const exit = formatMonitorEvent("watcher", "mon_1", "Monitor stopped.", "stopped");
    center.notify(exit, { passive: true, quietEvents: true });
    expect(sent).toHaveLength(0);
    expect(acknowledgedAt).toEqual([]);
    clock.advanceBy(5_000);
    idle = true;
    center.settled();
    expect(sent.map((entry) => entry.message.details)).toMatchObject([
      { event: "2 events\nbefore stop\nlast line", asOf: 1_000 },
      { status: "stopped", asOf: 1_000 },
    ]);
    expect(sent.map((entry) => entry.options)).toEqual([{ triggerTurn: false }, { triggerTurn: false }]);
    expect(acknowledgedAt).toEqual([1]);
    center.settled();
    expect(acknowledgedAt).toEqual([1]);
    center.dispose();
  });

  it("delivers a busy monitor's pending events before its exit notice", () => {
    // Seen in eval e2e (c2): `echo noop` exited while the agent was busy. The
    // exit notice went out at once, the coalesced "noop" only after the
    // agent settled, so the model heard "exited" before the line it printed.
    const sent: { message: { customType: string; content: string; details?: unknown }; options: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ message, options }),
      isIdle: () => false,
      clock: new ManualClock(),
    });
    center.notifyMonitorEvent("watcher", "mon_1", "noop");
    center.notifyMonitorEvent("other", "mon_2", "still going");
    const exit = formatMonitorEvent("watcher", "mon_1", "Monitor process exited (exit code 0).", "exited");
    center.notify({ customType: exit.customType, content: exit.content, details: exit.details });
    expect(sent.map((s) => s.message.details)).toMatchObject([
      { kind: "monitor", id: "mon_1", event: "noop" },
      { kind: "monitor", id: "mon_1", status: "exited" },
    ]);
    expect(sent.every((s) => (s.options as { deliverAs?: string }).deliverAs === "steer")).toBe(true);
    // Another monitor's pending events keep waiting for the agent to settle.
    expect(sent.some((s) => (s.message.details as { id?: string }).id === "mon_2")).toBe(false);
    center.dispose();
  });

  it("logs task wake batches, delivery mode, and deduplicated exits", () => {
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const center = new NotifyCenter({
      sendMessage: () => {},
      isIdle: () => false,
      clock: new ManualClock(),
      logEvent: (type, fields) => events.push({ type, fields }),
    });
    const exit = { taskId: "sh_1", kind: "shell", command: "true", status: "completed" as const, exitCode: 0, durationMs: 1, outputPath: "", preview: "" };
    center.notifyTaskExit(exit);
    center.notifyTaskExit(exit);
    center.flush();
    expect(events).toContainEqual({ type: "wake.dedupe", fields: { id: "sh_1" } });
    expect(events).toContainEqual({ type: "wake.emit", fields: { kind: "task", ids: ["sh_1"], batch: false } });
    expect(events).toContainEqual({ type: "wake.deliver", fields: { kind: "task", mode: "steer" } });
    center.dispose();
  });

  it("delivers immediately while idle and carries dropped-line metadata", () => {
    const sent: { message: { details?: unknown }; options: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ message, options }),
      isIdle: () => true,
      clock: new ManualClock(),
    });
    center.notifyMonitorEvent("ticker", "mon_1", "tick", 2);
    expect(sent).toHaveLength(1);
    expect(sent[0].message.details).toMatchObject({ kind: "monitor", event: "tick", droppedLines: 2 });
    center.dispose();
  });
});

describe("NotifyCenter stamps every wake with as-of", () => {
  // 2026-10-05 chief session: a subagent-done generated at 12:27:26Z reached
  // the model 800.9 s later with nothing saying how old it was.
  const T0 = Date.UTC(2026, 9, 5, 12, 27, 26, 61);

  function center(idle: () => boolean) {
    const clock = new ManualClock(T0);
    const sent: { message: { customType: string; content: string; details?: unknown }; options: unknown }[] = [];
    const c = new NotifyCenter({ sendMessage: (message, options) => sent.push({ message, options }), isIdle: idle, clock });
    return { clock, sent, c };
  }

  it("a notified wake carries the time it was generated, in content and details", () => {
    const { sent, c } = center(() => true);
    const wake = formatMonitorEvent("watcher", "mon_1", "ready");
    c.notify({ customType: wake.customType, content: wake.content, details: wake.details });
    expect(sent[0].message.content).toContain(
      '<pi-famulus-wake kind="monitor" id="mon_1" description="watcher" as-of="2026-10-05T12:27:26Z">',
    );
    expect(sent[0].message.details).toMatchObject({ kind: "monitor", asOf: T0 });
  });

  it("a held passive notice keeps the time it was built, not the time it went out", () => {
    let idle = false;
    const { clock, sent, c } = center(() => idle);
    const wake = formatMonitorEvent("watcher", "mon_1", "Monitor stopped.", "stopped");
    c.notify({ customType: wake.customType, content: wake.content, details: wake.details }, { passive: true });
    expect(sent).toHaveLength(0);
    clock.advanceBy(90_000);
    idle = true;
    c.settled();
    expect(sent).toHaveLength(1);
    expect(sent[0].message.details).toMatchObject({ asOf: T0 });
    expect(sent[0].message.content).toContain('status="stopped" as-of="2026-10-05T12:27:26Z">');
  });

  it("a task batch is stamped when it is built (window flush)", () => {
    const { clock, sent, c } = center(() => true);
    c.notifyTaskExit({
      taskId: "sh_1",
      kind: "shell",
      command: "true",
      status: "completed",
      exitCode: 0,
      durationMs: 5,
      outputPath: "/tmp/o",
      preview: "",
    });
    clock.advanceBy(1_000); // window is 200 ms; the batch is built at T0 + 200
    expect(sent).toHaveLength(1);
    expect(sent[0].message.details).toMatchObject({ kind: "task", asOf: T0 + 200 });
    expect(sent[0].message.content).toContain('<pi-famulus-wake kind="task" as-of="2026-10-05T12:27:26Z">');
  });

  it("leaves non-wake messages untouched", () => {
    const { sent, c } = center(() => true);
    c.notify({ customType: "something-else", content: '<pi-famulus-wake kind="x">', details: { kind: "x" } });
    expect(sent[0].message).toMatchObject({ content: '<pi-famulus-wake kind="x">', details: { kind: "x" } });
  });
});
