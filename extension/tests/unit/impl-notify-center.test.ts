import { AgentSession } from "@earendil-works/pi-coding-agent";
import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { formatMonitorEvent } from "../../src/format";
import { NotifyCenter } from "../../src/notify";
import { wakeIds, type FamulusWake } from "../../src/wake";

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

  it("does not split a surrogate pair at a preview boundary", () => {
    let idle = false;
    const sent: { details?: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => idle,
      clock: new ManualClock(),
    });
    const emoji = "😀";
    center.notifyMonitorEvent("noisy", "mon_1", `${"h".repeat(1449)}${emoji}${"t".repeat(2000)}`);
    idle = true;
    center.settled();
    const wake = sent[0].details as Extract<FamulusWake, { kind: "monitor" }>;
    const lone = [...wake.event].filter((ch) => {
      const code = ch.charCodeAt(0);
      return ch.length === 1 && code >= 0xd800 && code <= 0xdfff;
    });
    expect(lone).toEqual([]);
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
    ]);
    clock.advanceBy(5_000);
    expect(sent.map((entry) => entry.message.details)).toMatchObject([
      { event: "2 events\nbefore stop\nlast line", asOf: 1_000 },
      { kind: "monitor-terminal", asOf: 11_000 },
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

  it("queues one wake without inventing a hang during a long healthy host run", async () => {
    const queued: { content: unknown; details?: unknown }[] = [];
    // Exercise the installed host implementation, not a spy that assumes
    // triggerTurn can interrupt streaming. Both options enqueue agent.steer.
    const host = {
      isStreaming: true,
      agent: { steer: (message: typeof queued[number]) => queued.push(message) },
    } as unknown as AgentSession;
    const clock = new ManualClock();
    const center = new NotifyCenter({
      sendMessage: (message, options) => { void AgentSession.prototype.sendCustomMessage.call(host, message, options); },
      isIdle: () => !host.isStreaming,
      clock,
    });
    center.notify({
      customType: "pi-famulus-wake",
      content: "child finished",
      details: { kind: "subagent-done", runId: "run_1" },
    });
    expect(queued).toHaveLength(1);
    // A healthy multi-tool run can remain busy well beyond 45 seconds.
    clock.advanceBy(90_000);
    expect(queued.map((message) => message.details)).toEqual([
      expect.objectContaining({ kind: "subagent-done", runId: "run_1" }),
    ]);
    expect(queued[0].content).not.toMatch(/produced no tokens/);
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
    expect(sent[0].message.details).toMatchObject({ kind: "monitor-terminal", asOf: T0 + 10_000 });
    expect(sent[0].message.content).toContain('as-of="2026-10-05T12:27:36Z"');
    expect(sent[0].message.content).toContain('status="stopped"');
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

describe("NotifyCenter monitor terminal coalescing", () => {
  const WINDOW_MS = 10_000;

  function setup(idle: () => boolean = () => true) {
    const clock = new ManualClock(0);
    const sent: { message: { customType: string; content: string; details?: unknown }; options: unknown }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ message, options }),
      isIdle: idle,
      clock,
    });
    const terminal = (id: string, description: string, status: "timeout" | "exited" | "stopped", event: string, opts?: { passive?: boolean; quietEvents?: boolean }) => {
      const wake = formatMonitorEvent(description, id, event, status);
      center.notify({ customType: wake.customType, content: wake.content, details: wake.details }, opts);
    };
    return { clock, sent, center, terminal };
  }

  function drainMessages(drained: unknown): Array<{ details?: unknown; content?: string }> {
    if (Array.isArray(drained)) return drained as Array<{ details?: unknown; content?: string }>;
    if (drained && typeof drained === "object") return [drained as { details?: unknown; content?: string }];
    return [];
  }

  function drainKinds(drained: unknown): Array<string | undefined> {
    return drainMessages(drained).map((message) => (message.details as { kind?: string } | undefined)?.kind);
  }

  it("t0 and t9.9s join one batch listing every invalidated monitor", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "[Monitor timed out — re-arm if needed.]");
    clock.advanceBy(9_900);
    terminal("mon_b", "beta", "timeout", "[Monitor timed out — re-arm if needed.]");
    expect(sent).toHaveLength(0);
    clock.advanceBy(100);
    expect(sent).toHaveLength(1);
    const details = sent[0].message.details as FamulusWake;
    expect(details.kind).toBe("monitor-terminal");
    expect((details as { id?: string }).id).toBeUndefined();
    expect(wakeIds(details)).toEqual(["mon_a", "mon_b"]);
    expect(sent[0].message.content).toContain('kind="monitor-terminal"');
    expect(sent[0].message.content).toMatch(/These monitors have stopped; nothing watches their sources now/);
    expect(sent[0].message.content).toContain("alpha");
    expect(sent[0].message.content).toContain("beta");
    expect(sent[0].message.content).not.toMatch(/re-arm if needed/);
    expect(sent[0].options).toEqual({ triggerTurn: true });
    center.dispose();
  });

  it("t10.001 starts a second fixed window", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "stopped");
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(1);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_a"]);
    terminal("mon_b", "beta", "timeout", "stopped");
    clock.advanceBy(1);
    expect(sent).toHaveLength(1);
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(2);
    expect(wakeIds(sent[1].message.details as FamulusWake)).toEqual(["mon_b"]);
    center.dispose();
  });

  it("does not reset the window when a later same-class terminal joins", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "exited", "Monitor process exited");
    clock.advanceBy(9_000);
    terminal("mon_b", "beta", "exited", "Monitor process exited");
    clock.advanceBy(1_000);
    expect(sent).toHaveLength(1);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_a", "mon_b"]);
    clock.advanceBy(9_000);
    expect(sent).toHaveLength(1);
    center.dispose();
  });

  it("joins mixed timeout/exited/stopped in one fixed window and leaves output events out", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "timed out");
    center.notifyMonitorEvent("alpha", "mon_a_live", "tick-1");
    clock.advanceBy(9_900);
    terminal("mon_b", "beta", "exited", "Monitor process exited");
    clock.advanceBy(50);
    terminal("mon_c", "gamma", "stopped", "Monitor stopped.", { passive: true });
    expect(sent).toHaveLength(1);
    expect(sent[0].message.details).toMatchObject({ kind: "monitor", id: "mon_a_live", event: "tick-1" });
    clock.advanceBy(50);
    const terminals = sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal");
    expect(terminals).toHaveLength(1);
    const details = terminals[0].message.details as Extract<FamulusWake, { kind: "monitor-terminal" }>;
    expect(wakeIds(details)).toEqual(["mon_a", "mon_b", "mon_c"]);
    expect(details.monitors.map((monitor) => monitor.status)).toEqual(["timeout", "exited", "stopped"]);
    expect(details.monitors.map((monitor) => monitor.detail)).toEqual([
      "Timed out.",
      "Monitor process exited",
      "Monitor stopped.",
    ]);
    expect(terminals[0].options).toEqual({ triggerTurn: true });
    terminal("mon_d", "delta", "timeout", "timed out");
    clock.advanceBy(1);
    expect(sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal")).toHaveLength(1);
    clock.advanceBy(WINDOW_MS);
    const after = sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal");
    expect(after).toHaveLength(2);
    expect(wakeIds(after[1].message.details as FamulusWake)).toEqual(["mon_d"]);
    center.dispose();
  });

  it("flushes a monitor's own pending output before its terminal notice and leaves others pending", () => {
    let idle = false;
    const { clock, sent, center, terminal } = setup(() => idle);
    center.notifyMonitorEvent("watcher", "mon_1", "noop");
    center.notifyMonitorEvent("other", "mon_2", "still going");
    terminal("mon_1", "watcher", "exited", "Monitor process exited (exit code 0).");
    expect(sent.map((entry) => entry.message.details)).toMatchObject([
      { kind: "monitor", id: "mon_1", event: "noop" },
    ]);
    expect(sent.every((entry) => (entry.options as { deliverAs?: string }).deliverAs === "steer")).toBe(true);
    clock.advanceBy(WINDOW_MS);
    expect(sent.map((entry) => entry.message.details)).toMatchObject([
      { kind: "monitor", id: "mon_1", event: "noop" },
      { kind: "monitor-terminal" },
    ]);
    expect(wakeIds(sent[1].message.details as FamulusWake)).toEqual(["mon_1"]);
    expect(sent.some((entry) => (entry.message.details as { id?: string }).id === "mon_2")).toBe(false);
    idle = true;
    center.settled();
    expect(sent.some((entry) => (entry.message.details as { id?: string }).id === "mon_2")).toBe(true);
    center.dispose();
  });

  it("keeps caused-by-event and model-stopped notices passive after the window", () => {
    let idle = false;
    const { clock, sent, center, terminal } = setup(() => idle);
    terminal("mon_1", "watcher", "exited", "Monitor process exited", { passive: true, quietEvents: true });
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(0);
    idle = true;
    center.settled();
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: false });
    expect((sent[0].message.details as FamulusWake).kind).toBe("monitor-terminal");
    center.dispose();
  });

  it("flush() releases pending terminals immediately without waiting for the window", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "timed out");
    clock.advanceBy(3_000);
    terminal("mon_b", "beta", "exited", "Monitor process exited");
    expect(sent).toHaveLength(0);
    center.flush();
    expect(sent).toHaveLength(1);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_a", "mon_b"]);
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(1);
    center.dispose();
  });

  it("dispose flushes pending terminals instead of dropping them", () => {
    const { sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "timed out");
    expect(sent).toHaveLength(0);
    center.dispose();
    expect(sent).toHaveLength(1);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_a"]);
    expect(sent[0].message.content).toMatch(/<pi-famulus-wake kind="monitor-terminal"[^>]*>\s*<summary>These monitors have stopped; nothing watches their sources now\.<\/summary>/);
  });

  it("dispose delivers a busy passive terminal exactly once", () => {
    let idle = false;
    const { sent, center, terminal } = setup(() => idle);
    terminal("mon_a", "alpha", "exited", "Monitor process exited", { passive: true });
    expect(sent).toHaveLength(0);
    center.dispose();
    expect(sent.map((entry) => ({
      kind: (entry.message.details as { kind?: string }).kind,
      options: entry.options,
    }))).toEqual([
      { kind: "monitor-terminal", options: { triggerTurn: false } },
    ]);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_a"]);
    center.dispose();
    expect(sent).toHaveLength(1);
  });

  it("drainPendingMonitorTerminals returns the wake without sending through pi", () => {
    const { sent, center, terminal } = setup();
    terminal("mon_a", "alpha", "timeout", "timed out");
    const drained = center.drainPendingMonitorTerminals();
    expect(sent).toHaveLength(0);
    expect(drainKinds(drained)).toEqual(["monitor-terminal"]);
    expect(wakeIds(drainMessages(drained)[0]!.details as FamulusWake)).toEqual(["mon_a"]);
    center.dispose();
    expect(sent).toHaveLength(0);
  });

  it("drainPendingMonitorTerminals returns held own output then the terminal, without sending through pi", () => {
    let idle = false;
    const acknowledgedAt: number[] = [];
    const { sent, center, terminal } = setup(() => idle);
    const onDelivered = () => acknowledgedAt.push(sent.length);
    center.notifyMonitorEvent("quiet", "mon_q", "line-q", 0, { onDelivered });
    terminal("mon_q", "quiet", "exited", "Monitor process exited", { passive: true, quietEvents: true });
    terminal("mon_a", "active", "timeout", "timed out");
    const keep = formatMonitorEvent("kept", "mon_keep", "keep-me");
    center.notify({ customType: keep.customType, content: keep.content, details: keep.details }, { passive: true });
    const drained = center.drainPendingMonitorTerminals();
    expect(sent).toHaveLength(0);
    expect(drainKinds(drained)).toEqual(["monitor", "monitor-terminal"]);
    const messages = drainMessages(drained);
    expect((messages[0]?.details as { id?: string }).id).toBe("mon_q");
    expect(JSON.stringify(messages[0]?.details)).toContain("line-q");
    expect(wakeIds(messages[1]?.details as FamulusWake)).toEqual(["mon_q", "mon_a"]);
    expect(acknowledgedAt).toEqual([0]);
    idle = true;
    center.settled();
    expect(sent.map((entry) => entry.message.details)).toMatchObject([{ kind: "monitor", id: "mon_keep" }]);
    expect(acknowledgedAt).toEqual([0]);
    center.dispose();
    expect(sent).toHaveLength(1);
    expect(acknowledgedAt).toEqual([0]);
  });

  it("file drain takes a timer-flushed busy passive terminal from held, without sending through pi", () => {
    let idle = false;
    const { clock, sent, center, terminal } = setup(() => idle);
    center.notifyMonitorEvent("quiet", "mon_q", "line-q", 0, { quiet: true });
    terminal("mon_q", "quiet", "exited", "Monitor process exited", { passive: true, quietEvents: true });
    expect(sent).toHaveLength(0);
    clock.advanceBy(WINDOW_MS);
    expect(sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal")).toHaveLength(0);
    const drained = center.drainPendingMonitorTerminals();
    expect(sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal")).toHaveLength(0);
    expect(drainKinds(drained)).toEqual(["monitor", "monitor-terminal"]);
    const messages = drainMessages(drained);
    expect((messages[0]?.details as { id?: string }).id).toBe("mon_q");
    expect(JSON.stringify(messages[0]?.details)).toContain("line-q");
    expect(wakeIds(messages[1]?.details as FamulusWake)).toEqual(["mon_q"]);
    const again = center.drainPendingMonitorTerminals();
    expect(drainKinds(again)).toEqual([]);
    center.dispose();
    expect(sent.filter((entry) => (entry.message.details as { kind?: string }).kind === "monitor-terminal")).toHaveLength(0);
  });

  it("file drain does not re-acknowledge output already delivered live", () => {
    let idle = false;
    let acknowledged = 0;
    const { sent, center, terminal } = setup(() => idle);
    const onDelivered = () => { acknowledged++; };
    center.notifyMonitorEvent("live", "mon_n", "line-n", 0, { onDelivered });
    terminal("mon_n", "live", "exited", "Monitor process exited");
    expect(sent.map((entry) => (entry.message.details as { kind?: string }).kind)).toEqual(["monitor"]);
    expect(acknowledged).toBe(1);
    const drained = center.drainPendingMonitorTerminals();
    expect(drainKinds(drained)).toEqual(["monitor-terminal"]);
    expect(acknowledged).toBe(1);
    center.dispose();
    expect(acknowledged).toBe(1);
    expect(sent).toHaveLength(1);
  });

  it("merges a same-id exited/stopped race into one richer item", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_1", "watcher", "exited", "Monitor process exited (exit code 0).");
    terminal("mon_1", "watcher rate-limit", "stopped", "Monitor stopped.", {
      // second notice carries output metadata the first lacked
    });
    const withOutput = formatMonitorEvent("watcher", "mon_1", "Monitor stopped.", "stopped", {
      droppedLines: 4,
      output: { cursorStart: 0, cursorEnd: 12, suppressedCount: 2, firstObservedAt: 1, lastObservedAt: 2 },
    });
    center.notify({ customType: withOutput.customType, content: withOutput.content, details: withOutput.details });
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(1);
    const details = sent[0].message.details as Extract<FamulusWake, { kind: "monitor-terminal" }>;
    expect(wakeIds(details)).toEqual(["mon_1"]);
    expect(details.monitors).toHaveLength(1);
    expect(details.monitors[0]?.status).toBe("stopped");
    expect(details.monitors[0]?.droppedLines).toBe(4);
    expect(details.monitors[0]?.output?.suppressedCount).toBe(2);
    expect(details.monitors[0]?.detail).toBe("Monitor process exited (exit code 0).");
    center.dispose();
  });

  it("does not open a second window for a late duplicate of an already delivered id", () => {
    const { clock, sent, center, terminal } = setup();
    terminal("mon_1", "watcher", "exited", "Monitor process exited");
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(1);
    terminal("mon_1", "watcher", "stopped", "Monitor stopped.");
    clock.advanceBy(WINDOW_MS);
    expect(sent).toHaveLength(1);
    expect(wakeIds(sent[0].message.details as FamulusWake)).toEqual(["mon_1"]);
    center.dispose();
  });

  it("drains held quiet output before a mixed terminal batch without starting an extra turn", () => {
    let idle = false;
    const { clock, sent, center, terminal } = setup(() => idle);
    center.notifyMonitorEvent("quiet", "mon_q", "line-q");
    terminal("mon_q", "quiet", "exited", "Monitor process exited", { passive: true, quietEvents: true });
    terminal("mon_a", "active", "timeout", "timed out");
    expect(sent).toHaveLength(0);
    clock.advanceBy(WINDOW_MS);
    expect(sent.map((entry) => ({
      kind: (entry.message.details as { kind?: string }).kind,
      options: entry.options,
    }))).toEqual([
      { kind: "monitor", options: { triggerTurn: false } },
      { kind: "monitor-terminal", options: { deliverAs: "steer" } },
    ]);
    expect((sent[0].message.details as { id?: string }).id).toBe("mon_q");
    expect(JSON.stringify(sent[0].message.details)).toContain("line-q");
    expect(wakeIds(sent[1].message.details as FamulusWake)).toEqual(["mon_q", "mon_a"]);
    center.dispose();
  });
});
