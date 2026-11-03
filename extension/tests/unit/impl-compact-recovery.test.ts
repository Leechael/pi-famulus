/**
 * Compact recovery: idle flush after session_compact, running-monitor list,
 * one turn for a terminal notice the model has not seen.
 */
import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { formatMonitorEvent, formatRunningMonitorList } from "../../src/format";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
  NotifyCenter,
  attachCompactRecovery,
  scheduleIdleCheck,
} from "../../src/notify";
import { wakeIds, type FamulusWake } from "../../src/wake";

function flush(ms = 0): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function terminalNotice(id = "mon_1", event = "Monitor process exited (exit code 0).") {
  const wake = formatMonitorEvent("watcher", id, event, "exited");
  return { customType: wake.customType, content: wake.content, details: wake.details };
}

describe("scheduleIdleCheck", () => {
  it("runs on the next tick once idle with an empty queue", async () => {
    let idle = false;
    let pending = false;
    let ready = 0;
    const handle = scheduleIdleCheck({
      isReady: () => idle && !pending,
      onReady: () => {
        ready++;
      },
    });
    await flush(0);
    expect(ready).toBe(0);
    idle = true;
    await flush(30);
    expect(ready).toBe(1);
    await flush(30);
    expect(ready).toBe(1);
    handle.cancel();
  });

  it("retries every 25ms while not idle, then fires once", async () => {
    let idle = false;
    let ready = 0;
    const handle = scheduleIdleCheck({
      isReady: () => idle,
      onReady: () => {
        ready++;
      },
    });
    await flush(0);
    expect(ready).toBe(0);
    await flush(20);
    expect(ready).toBe(0);
    idle = true;
    await flush(40);
    expect(ready).toBe(1);
    handle.cancel();
  });

  it("cancel on a new run never calls onReady", async () => {
    let idle = false;
    let ready = 0;
    const handle = scheduleIdleCheck({
      isReady: () => idle,
      onReady: () => {
        ready++;
      },
    });
    handle.cancel();
    idle = true;
    await flush(40);
    expect(ready).toBe(0);
  });
});

describe("NotifyCenter compact hold and one-turn flush", () => {
  it("holds event then terminal during compact; list is passive and appended before the one triggerTurn", () => {
    const sent: { details?: unknown; options: { triggerTurn?: boolean; deliverAs?: "steer" } }[] = [];
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const clock = new ManualClock();
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
      isIdle: () => false,
      clock,
      logEvent: (type, fields) => events.push({ type, fields }),
    });
    center.beginHold();
    center.notifyMonitorEvent("watcher", "mon_1", "noop");
    center.notify(terminalNotice());
    expect(sent).toHaveLength(0);

    const extra = formatRunningMonitorList([{ id: "mon_2", description: "ticker", remaining: "2m remaining" }]);
    center.settledAfterCompact({ customType: extra.customType, content: extra.content, details: extra.details });

    // The passive list precedes the final active wake, so the whole batch is
    // appended before one turn and the list itself never triggers it.
    const kinds = sent.map((entry) => (entry.details as FamulusWake).kind);
    expect(kinds).toEqual(["monitor", "running-monitors", "monitor-terminal"]);
    expect(sent.map((entry) => entry.options.triggerTurn === true)).toEqual([false, false, true]);
    expect(sent.filter((entry) => entry.options.triggerTurn === true)).toHaveLength(1);
    const event = sent[0].details as Extract<FamulusWake, { kind: "monitor" }>;
    expect(event.event).toBe("noop");
    const terminal = sent[2].details as Extract<FamulusWake, { kind: "monitor-terminal" }>;
    expect(terminal.monitors.map((monitor) => monitor.status)).toEqual(["exited"]);
    expect(wakeIds(terminal)).toEqual(["mon_1"]);
    const list = sent[1].details as Extract<FamulusWake, { kind: "running-monitors" }>;
    expect(list.monitors.map((item) => item.id)).toEqual(["mon_2"]);
    expect(wakeIds(list)).toEqual(["mon_2"]);
    expect(events.filter((event) => event.type === "wake.deliver").map((event) => event.fields?.mode)).toEqual([
      "append",
      "passive",
      "trigger",
    ]);
    center.dispose();
  });

  it("does not send when compact produced nothing to flush", () => {
    const sent: unknown[] = [];
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => false,
      clock: new ManualClock(),
    });
    center.beginHold();
    center.settledAfterCompact();
    expect(sent).toHaveLength(0);
    center.dispose();
  });

  it("a terminal-only compact flush still uses exactly one triggerTurn", () => {
    const sent: { options: { triggerTurn?: boolean } }[] = [];
    const center = new NotifyCenter({
      sendMessage: (_message, options) => sent.push({ options }),
      isIdle: () => false,
      clock: new ManualClock(),
    });
    center.beginHold();
    center.notify(terminalNotice());
    center.settledAfterCompact();
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: true });
    center.dispose();
  });

  it("a running-monitor list alone is appended with no turn", () => {
    const sent: { details?: unknown; options: { triggerTurn?: boolean } }[] = [];
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
      isIdle: () => true,
      clock: new ManualClock(),
    });
    const extra = formatRunningMonitorList([
      { id: "mon_1", description: "ticker", remaining: "2m remaining" },
      { id: "mon_2", description: "watcher", remaining: "persistent" },
    ]);
    center.settledAfterCompact({ customType: extra.customType, content: extra.content, details: extra.details });
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: false });
    expect((sent[0].details as FamulusWake).kind).toBe("running-monitors");
    center.dispose();
  });

  it("endHold then settled() still triggerTurns a nonpassive held wake", () => {
    const sent: { options: { triggerTurn?: boolean } }[] = [];
    const center = new NotifyCenter({
      sendMessage: (_message, options) => sent.push({ options }),
      isIdle: () => true,
      clock: new ManualClock(),
    });
    center.beginHold();
    center.notifyMonitorEvent("watcher", "mon_1", "noop");
    center.endHold();
    expect(sent).toHaveLength(0);
    center.settled();
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: true });
    center.dispose();
  });

  it("active timeout during holdAll stays in the terminal queue; compact idle flushes output then terminal once", () => {
    const sent: { details?: unknown; options: { triggerTurn?: boolean } }[] = [];
    const clock = new ManualClock();
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
      isIdle: () => true,
      clock,
    });
    center.beginHold();
    center.notifyMonitorEvent("watcher", "mon_1", "tick-line");
    const timeout = formatMonitorEvent("watcher", "mon_1", "[Monitor timed out — re-arm if needed.]", "timeout");
    center.notify({ customType: timeout.customType, content: timeout.content, details: timeout.details });
    clock.advanceBy(10_000);
    expect(sent).toHaveLength(0);
    center.settledAfterCompact();
    expect(sent.map((entry) => (entry.details as FamulusWake).kind)).toEqual(["monitor", "monitor-terminal"]);
    const output = sent[0].details as Extract<FamulusWake, { kind: "monitor" }>;
    expect(output.event).toContain("tick-line");
    const terminal = sent[1].details as Extract<FamulusWake, { kind: "monitor-terminal" }>;
    expect(wakeIds(terminal)).toEqual(["mon_1"]);
    expect(terminal.monitors.map((monitor) => monitor.status)).toEqual(["timeout"]);
    expect(JSON.stringify(terminal)).not.toMatch(/re-arm if needed/);
    expect(sent.map((entry) => entry.options.triggerTurn === true)).toEqual([false, true]);
    expect(sent.filter((entry) => (entry.details as FamulusWake).kind === "monitor-terminal")).toHaveLength(1);
    center.dispose();
    expect(sent.filter((entry) => (entry.details as FamulusWake).kind === "monitor-terminal")).toHaveLength(1);
  });

  it("passive timeout during holdAll flushes output then terminal once with no turn", () => {
    const sent: { details?: unknown; options: { triggerTurn?: boolean } }[] = [];
    const clock = new ManualClock();
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
      isIdle: () => true,
      clock,
    });
    center.beginHold();
    center.notifyMonitorEvent("watcher", "mon_1", "tick-line", 0, { quiet: true });
    const timeout = formatMonitorEvent("watcher", "mon_1", "[Monitor timed out — re-arm if needed.]", "timeout");
    center.notify(
      { customType: timeout.customType, content: timeout.content, details: timeout.details },
      { passive: true, quietEvents: true },
    );
    clock.advanceBy(10_000);
    expect(sent).toHaveLength(0);
    const extra = formatRunningMonitorList([{ id: "mon_2", description: "ticker", remaining: "persistent" }]);
    center.settledAfterCompact({ customType: extra.customType, content: extra.content, details: extra.details });
    expect(sent.map((entry) => (entry.details as FamulusWake).kind)).toEqual([
      "monitor",
      "monitor-terminal",
      "running-monitors",
    ]);
    const output = sent[0].details as Extract<FamulusWake, { kind: "monitor" }>;
    expect(output.event).toContain("tick-line");
    const terminal = sent[1].details as Extract<FamulusWake, { kind: "monitor-terminal" }>;
    expect(wakeIds(terminal)).toEqual(["mon_1"]);
    expect(terminal.monitors.map((monitor) => monitor.status)).toEqual(["timeout"]);
    expect(sent.every((entry) => entry.options.triggerTurn !== true)).toBe(true);
    expect(sent.filter((entry) => (entry.details as FamulusWake).kind === "monitor-terminal")).toHaveLength(1);
    center.dispose();
    expect(sent.filter((entry) => (entry.details as FamulusWake).kind === "monitor-terminal")).toHaveLength(1);
  });
});

describe("formatRunningMonitorList", () => {
  it("lists every id and is not a single-monitor event", () => {
    const wake = formatRunningMonitorList([
      { id: "mon_1", description: "ticker", remaining: "2m remaining" },
      { id: "mon_2", description: "watcher", remaining: "persistent" },
    ]);
    expect(wake.details).toMatchObject({ kind: "running-monitors" });
    expect(wakeIds(wake.details)).toEqual(["mon_1", "mon_2"]);
    expect(wake.content).toContain('kind="running-monitors"');
    expect(wake.content).toContain("informational list");
    expect(wake.content).toContain("not an event");
    expect(wake.content).toContain("No response is needed");
    expect(wake.content).toContain("do not use it to poll");
    expect(wake.content).toContain('id="mon_1"');
    expect(wake.content).toContain('id="mon_2"');
    expect(wake.content).toContain("ticker");
    expect(wake.content).toContain("persistent");
    expect((wake.details as { id?: string }).id).toBeUndefined();
  });
});

describe("attachCompactRecovery", () => {
  function fakePi() {
    const handlers = new Map<string, (event: unknown, ctx: ExtensionContext) => unknown>();
    const pi = {
      on(event: string, handler: (event: unknown, ctx: ExtensionContext) => unknown) {
        handlers.set(event, handler);
        return () => handlers.delete(event);
      },
    };
    return { pi: pi as Pick<ExtensionAPI, "on">, handlers };
  }

  function ctxState(state: { idle: boolean; pending?: boolean; dead?: boolean }) {
    return {
      isIdle: () => !state.dead && state.idle,
      hasPendingMessages: () => state.pending === true,
    } as ExtensionContext;
  }

  it("session_compact and session_compact_failed flush after idle; new run cancels", async () => {
    const sent: { options: { triggerTurn?: boolean } }[] = [];
    const clock = new ManualClock();
    const center = new NotifyCenter({
      sendMessage: (_message, options) => sent.push({ options }),
      isIdle: () => state.idle,
      clock,
    });
    const state = { idle: false, pending: false };
    const running: { id: string; description: string; remaining: string }[] = [];
    const { pi, handlers } = fakePi();
    const recovery = attachCompactRecovery(pi, {
      getCenter: () => center,
      getCtx: () => ctxState(state),
      listRunningMonitors: () => running,
    });

    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice());
    expect(sent).toHaveLength(0);

    await handlers.get("session_compact")?.({ reason: "manual" }, ctxState(state));
    await flush(0);
    expect(sent).toHaveLength(0);
    state.idle = true;
    await flush(40);
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: true });

    sent.length = 0;
    state.idle = false;
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice("mon_2"));
    await handlers.get("session_compact_failed")?.({ reason: "manual", aborted: true }, ctxState(state));
    await handlers.get("before_agent_start")?.({ systemPrompt: "x" }, ctxState(state));
    state.idle = true;
    await flush(40);
    expect(sent).toHaveLength(0);

    recovery.dispose();
    center.dispose();
  });

  it("injects one running-monitor list after compaction without starting a turn", async () => {
    const sent: { details?: unknown; options: { triggerTurn?: boolean } }[] = [];
    const clock = new ManualClock();
    const state = { idle: false };
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
      isIdle: () => state.idle,
      clock,
    });
    const { pi, handlers } = fakePi();
    const recovery = attachCompactRecovery(pi, {
      getCenter: () => center,
      getCtx: () => ctxState(state),
      listRunningMonitors: () => [
        { id: "mon_1", description: "ticker", remaining: "4m remaining" },
        { id: "mon_2", description: "watch", remaining: "persistent" },
      ],
    });
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    await handlers.get("session_compact")?.({ reason: "manual" }, ctxState(state));
    state.idle = true;
    await flush(40);
    expect(sent).toHaveLength(1);
    expect((sent[0].details as FamulusWake).kind).toBe("running-monitors");
    expect(wakeIds(sent[0].details as FamulusWake)).toEqual(["mon_1", "mon_2"]);
    expect(sent[0].options).toEqual({ triggerTurn: false });
    recovery.dispose();
    center.dispose();
  });

  it("session replacement and teardown cancel a pending idle check", async () => {
    const sent: unknown[] = [];
    const state = { idle: false };
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => state.idle,
      clock: new ManualClock(),
    });
    const { pi, handlers } = fakePi();
    const recovery = attachCompactRecovery(pi, {
      getCenter: () => center,
      getCtx: () => ctxState(state),
      listRunningMonitors: () => [],
    });
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice());
    await handlers.get("session_compact")?.({ reason: "manual" }, ctxState(state));
    recovery.dispose();
    state.idle = true;
    await flush(40);
    expect(sent).toHaveLength(0);
    center.dispose();
  });

  it("releases held wakes if compact never emits compact/failed and the session stays idle", async () => {
    const sent: { options: { triggerTurn?: boolean } }[] = [];
    const state = { idle: true };
    const center = new NotifyCenter({
      sendMessage: (_message, options) => sent.push({ options }),
      isIdle: () => state.idle,
      clock: new ManualClock(),
    });
    const { pi, handlers } = fakePi();
    const recovery = attachCompactRecovery(pi, {
      getCenter: () => center,
      getCtx: () => ctxState(state),
      listRunningMonitors: () => [],
      idleHoldMs: 40,
      maxHoldMs: 5_000,
    });
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice());
    expect(sent).toHaveLength(0);
    await flush(20);
    expect(sent).toHaveLength(0);
    await flush(80);
    expect(sent).toHaveLength(1);
    expect(sent[0].options).toEqual({ triggerTurn: true });
    recovery.dispose();
    center.dispose();
  });

  it("releases held wakes at the absolute cap even while busy, and new run clears the watchdog", async () => {
    const sent: unknown[] = [];
    const state = { idle: false };
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => state.idle,
      clock: new ManualClock(),
    });
    const { pi, handlers } = fakePi();
    const recovery = attachCompactRecovery(pi, {
      getCenter: () => center,
      getCtx: () => ctxState(state),
      listRunningMonitors: () => [],
      idleHoldMs: 40,
      maxHoldMs: 60,
    });
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice());
    await flush(120);
    expect(sent.length).toBeGreaterThan(0);

    sent.length = 0;
    await handlers.get("session_before_compact")?.({ reason: "manual" }, ctxState(state));
    center.notify(terminalNotice("mon_cap"));
    await handlers.get("before_agent_start")?.({}, ctxState(state));
    await flush(150);
    expect(sent).toHaveLength(0);
    recovery.dispose();
    await flush(80);
    expect(sent).toHaveLength(0);
    center.dispose();
  });
});
