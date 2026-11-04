import { describe, expect, it, vi } from "vitest";
import { ManualClock } from "../../src/clock";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ManagerClient, MonitorOutputRecord } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import type { OverloadControllerOptions } from "../../src/monitor-batching";
import { NotifyCenter } from "../../src/notify";
import { createTaskStopTool } from "../../src/task-tools";
import type { FamulusWake } from "../../src/wake";

const TERMINAL_MS = 10_000;

function terminalStatus(details: unknown): string | undefined {
  const d = details as { kind?: string; status?: string; monitors?: { status: string }[] } | undefined;
  if (d?.kind === "monitor-terminal") return d.monitors?.[0]?.status;
  return d?.status;
}

describe("MonitorRegistry saturation", () => {
  async function setup(overload?: Partial<OverloadControllerOptions>, start: { dedup_window_ms?: number } = { dedup_window_ms: 0 }) {
    const clock = new ManualClock();
    const stopped: string[] = [];
    const sent: { details?: unknown }[] = [];
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      start: async () => ({ task_id: "mon_1", pid: 123 }),
      watch: async () => {},
      stop: async (taskId: string, reason?: string) => { stopped.push(reason ? `${taskId}:${reason}` : taskId); },
    } as unknown as ManagerClient;
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => true,
      clock,
    });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
      logEvent: (type, fields) => events.push({ type, fields }),
      ...(overload !== undefined ? { overload } : {}),
    });
    const { taskId } = await registry.start(
      { command: "ticker", description: "ticker", persistent: true, ...start },
      { cwd: "/tmp" } as ExtensionContext,
    );
    const wakes = () => sent.map((item) => item.details as FamulusWake | undefined).filter(
      (details): details is Extract<FamulusWake, { kind: "monitor" }> => details?.kind === "monitor",
    );
    const terminals = () => sent.map((item) => item.details as FamulusWake | undefined).filter(
      (details): details is Extract<FamulusWake, { kind: "monitor-terminal" }> => details?.kind === "monitor-terminal",
    );
    return { clock, registry, center, stopped, events, taskId, wakes, sent, terminals };
  }

  it("enters summary on sustained drops even when accepted batches interrupt them, and does not stop", async () => {
    const { clock, registry, center, stopped, events, taskId, wakes } = await setup();
    for (let i = 0; i < 151; i++) {
      registry.handleOutput(taskId, `line ${i}\n`);
      clock.advanceBy(200);
      await Promise.resolve();
      await Promise.resolve();
    }
    expect(stopped).toEqual([]);
    expect(registry.has("mon_1")).toBe(true);
    expect(events.some((event) => event.type === "monitor.drop" && event.fields?.id === "mon_1")).toBe(true);
    expect(events.some((event) => event.type === "monitor.stop")).toBe(false);
    for (let i = 0; i < 150; i++) {
      registry.handleOutput(taskId, `line extra ${i}\n`);
      clock.advanceBy(200);
      await Promise.resolve();
    }
    const summary = wakes().find((w) => w.event.includes("overload summary"));
    expect(summary).toBeDefined();
    expect(summary?.status).not.toBe("stopped");
    expect(stopped).toEqual([]);
    expect(registry.has("mon_1")).toBe(true);
    center.dispose();
    registry.disposeAll();
  });

  it("flushes a pending summary before the terminal exit notice", async () => {
    const { clock, registry, center, taskId, wakes, sent } = await setup({
      exitQuietMs: 3_600_000,
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      cadenceMs: [60_000, 60_000, 60_000, 60_000],
    });
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(1_000);
    await Promise.resolve();
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(200);
    await Promise.resolve();
    expect(wakes().some((w) => w.event.includes("overload summary"))).toBe(false);
    registry.handleExit(taskId, { event: "task_exited", task_id: taskId, exit_code: 0, duration_ms: 50 });
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    const order = sent.map((item) => {
      const details = item.details as FamulusWake;
      if (details.kind === "monitor" && details.event.includes("overload summary")) return "summary";
      if (details.kind === "monitor-terminal") return "terminal";
      return details.kind;
    });
    expect(order.indexOf("summary")).toBeGreaterThanOrEqual(0);
    expect(order.indexOf("terminal")).toBeGreaterThan(order.indexOf("summary"));
    center.dispose();
    registry.disposeAll();
  });

  it("stops only after sustained extreme input line rate, with a clear backstop notice", async () => {
    const { clock, registry, center, stopped, events, taskId, wakes, terminals } = await setup({
      stopLinesPerSec: 20,
      stopSustainMs: 2_000,
      windowMs: 1_000,
      summaryLinesPerSec: 100,
    });
    for (let s = 0; s < 3; s++) {
      registry.handleOutput(taskId, `${"flood\n".repeat(20)}`);
      clock.advanceBy(1_000);
      await Promise.resolve();
      await Promise.resolve();
    }
    await Promise.resolve();
    await Promise.resolve();
    expect(stopped.length).toBeGreaterThan(0);
    expect(stopped.some((s) => s.includes("rate-limit"))).toBe(true);
    clock.advanceBy(TERMINAL_MS);
    const stopWake = terminals().find((wake) => wake.monitors.some((monitor) => monitor.status === "stopped"));
    expect(stopWake?.monitors[0]?.detail).toMatch(/1000 lines\/s|lines\/s for 60s|safety backstop/i);
    expect(stopWake?.monitors[0]?.detail).toMatch(/Stop requested; exit unconfirmed/);
    expect(stopWake?.monitors[0]?.detail).not.toMatch(/monitor stopped|was stopped/i);
    expect(events).toContainEqual({ type: "monitor.stop", fields: { id: "mon_1", reason: "rate-limit" } });
    center.dispose();
    registry.disposeAll();
  });

  it("steady distinct 2 lines/s for 30s enters summary via drop-ratio; 1 line/s does not enter in the first 30s", async () => {
    const two = await setup();
    for (let i = 0; i < 61; i++) {
      two.registry.handleOutput(two.taskId, `line ${i}\n`);
      two.clock.advanceBy(500);
      await Promise.resolve();
    }
    const twoEvents = two.wakes().map((w) => w.event);
    expect(two.stopped).toEqual([]);
    expect(two.registry.has("mon_1")).toBe(true);
    expect(twoEvents.filter((e) => e.includes("entering summary"))).toHaveLength(1);
    expect(twoEvents.some((e) => e.includes("drop-ratio"))).toBe(true);
    expect(twoEvents.find((e) => e.includes("entering summary"))).not.toMatch(/\d+ lines\/s/);
    expect(twoEvents.filter((e) => e.includes("leaving summary"))).toHaveLength(0);
    two.center.dispose();
    two.registry.disposeAll();

    const one = await setup();
    for (let i = 0; i < 32; i++) {
      one.registry.handleOutput(one.taskId, `line ${i}\n`);
      one.clock.advanceBy(1000);
      await Promise.resolve();
    }
    expect(one.wakes().some((w) => w.event.includes("entering summary"))).toBe(false);
    expect(one.stopped).toEqual([]);
    one.center.dispose();
    one.registry.disposeAll();
  });

  it("duplicate 5 lines/s with Rust records does not consume the drop budget; INPUT stays pre-dedup so =5/s does not enter", async () => {
    const { clock, registry, center, stopped, events, taskId, wakes } = await setup(undefined, {});
    let cursor = 0;
    const rec = (at: number): MonitorOutputRecord => {
      const text = "dup";
      const chunk = `${text}\n`;
      cursor += Buffer.byteLength(chunk);
      return {
        version: 1,
        kind: "line",
        observed_at: at,
        cursor_start: cursor - Buffer.byteLength(chunk),
        cursor_end: cursor,
        preview: text,
        truncated: false,
        fingerprint: "blake3:v1:dup",
        timestamp_fingerprint: "blake3:iso-edge-v1:dup",
      };
    };
    for (let s = 0; s <= 30; s++) {
      clock.advanceTo(s * 1000);
      for (let k = 0; k < 5; k++) {
        const record = rec(s);
        registry.handleOutput(taskId, "dup\n", record.cursor_end, [record]);
      }
      clock.advanceBy(200);
      await Promise.resolve();
    }
    expect(events.some((e) => e.type === "monitor.drop")).toBe(false);
    expect(wakes().some((w) => w.event.includes("entering summary"))).toBe(false);
    expect(stopped).toEqual([]);
    center.dispose();
    registry.disposeAll();
  });

  it("duplicate 6 lines/s with Rust records enters via pre-dedup line-rate without consuming drop budget", async () => {
    const { clock, registry, center, stopped, events, taskId, wakes } = await setup(undefined, {});
    let cursor = 0;
    const rec = (at: number): MonitorOutputRecord => {
      cursor += 4;
      return {
        version: 1,
        kind: "line",
        observed_at: at,
        cursor_start: cursor - 4,
        cursor_end: cursor,
        preview: "dup",
        truncated: false,
        fingerprint: "blake3:v1:dup",
        timestamp_fingerprint: "blake3:iso-edge-v1:dup",
      };
    };
    for (let s = 0; s <= 30; s++) {
      clock.advanceTo(s * 1000);
      for (let k = 0; k < 6; k++) {
        const record = rec(s);
        registry.handleOutput(taskId, "dup\n", record.cursor_end, [record]);
      }
      clock.advanceBy(200);
      await Promise.resolve();
    }
    const enter = wakes().filter((w) => w.event.includes("entering summary"));
    expect(enter).toHaveLength(1);
    expect(enter[0].event).toMatch(/line-rate|lines\/s/);
    expect(events.some((e) => e.type === "monitor.drop")).toBe(false);
    expect(stopped).toEqual([]);
    center.dispose();
    registry.disposeAll();
  });

  it("emits exactly one ENTER (bypassing RateLimiter) then one EXIT, keeping event-before-terminal", async () => {
    const { clock, registry, center, taskId, wakes, sent } = await setup({
      exitQuietMs: 3_600_000,
      windowMs: 1_000,
      summaryLinesPerSec: 2,
    });
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(1_000);
    await Promise.resolve();
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(200);
    await Promise.resolve();
    const afterEnter = wakes().map((w) => w.event);
    expect(afterEnter.filter((e) => e.includes("entering summary"))).toHaveLength(1);
    expect(afterEnter.some((e) => e.includes("Individual lines are suppressed"))).toBe(true);
    expect(afterEnter.some((e) => e.includes("task_output"))).toBe(true);
    expect(afterEnter.some((e) => e.includes("overload summary"))).toBe(false);
    clock.advanceBy(30_000);
    expect(wakes().filter((w) => w.event.includes("overload summary"))).toHaveLength(1);
    expect(wakes().filter((w) => w.event.includes("entering summary"))).toHaveLength(1);
    registry.handleExit(taskId, { event: "task_exited", task_id: taskId, exit_code: 0, duration_ms: 50 });
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    const order = sent.map((item) => {
      const details = item.details as FamulusWake;
      if (details.kind === "monitor" && details.event.includes("entering summary")) return "enter";
      if (details.kind === "monitor" && details.event.includes("overload summary")) return "summary";
      if (details.kind === "monitor-terminal") return "terminal";
      return details.kind;
    });
    expect(order.indexOf("summary")).toBeGreaterThan(order.indexOf("enter"));
    expect(order.indexOf("terminal")).toBeGreaterThan(order.indexOf("summary"));
    center.dispose();
    registry.disposeAll();
  });

  it("does not join a raw partial across metadata takeover", async () => {
    const { clock, registry, center, taskId, wakes } = await setup({
      windowMs: 1_000,
      summaryLinesPerSec: 0.5,
      exitQuietMs: 3_600_000,
    });
    registry.handleOutput(taskId, "HTTP ", 5);
    const record: MonitorOutputRecord = {
      version: 1,
      kind: "line",
      observed_at: clock.now(),
      cursor_start: 0,
      cursor_end: 9,
      preview: "HTTP 500",
      truncated: false,
    };
    registry.handleOutput(taskId, "HTTP 500\n", 9, [record]);
    clock.advanceBy(1_000);
    await Promise.resolve();
    registry.handleOutput(taskId, "next\n", 14);
    clock.advanceBy(200);
    await Promise.resolve();
    expect(wakes().some((w) => w.event.includes("entering summary"))).toBe(true);
    clock.advanceBy(30_000);
    const summary = wakes().find((w) => w.event.includes("overload summary"));
    expect(summary?.event).toMatch(/lines=1/);
    expect(summary?.event).toMatch(/bytes=5/);
    expect(summary?.event).toMatch(/cursor=9-14/);
    expect(summary?.event).toMatch(/next x1/);
    expect(summary?.event).not.toMatch(/HTTP next/);
    expect(summary?.event).not.toMatch(/bytes=10/);
    expect(summary?.event).not.toMatch(/cursor=0-14/);
    center.dispose();
    registry.disposeAll();
  });

  it("legacy split HTTP + 500\\n counts one HTTP 500 line of 9 bytes", async () => {
    const { clock, registry, center, taskId, wakes } = await setup({
      windowMs: 1_000,
      summaryLinesPerSec: 0.5,
      exitQuietMs: 3_600_000,
    });
    registry.handleOutput(taskId, "HTTP ");
    clock.advanceBy(50);
    registry.handleOutput(taskId, "500\n");
    clock.advanceBy(1_000);
    await Promise.resolve();
    registry.handleOutput(taskId, "HTTP 500\n");
    clock.advanceBy(200);
    await Promise.resolve();
    expect(wakes().some((w) => w.event.includes("entering summary"))).toBe(true);
    clock.advanceBy(30_000);
    const summary = wakes().find((w) => w.event.includes("overload summary"));
    expect(summary?.event).toMatch(/HTTP 500/);
    expect(summary?.event).not.toMatch(/(^|\n)500 x/);
    expect(summary?.event).toMatch(/raw via task_output cursor=/);
    center.dispose();
    registry.disposeAll();
  });

  it("emits exactly one EXIT notice on quiet exit", async () => {
    const { clock, registry, center, taskId, wakes } = await setup({
      windowMs: 1_000,
      summaryLinesPerSec: 2,
      exitQuietMs: 5_000,
    });
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(1_000);
    await Promise.resolve();
    registry.handleOutput(taskId, `${"HTTP 500\n".repeat(10)}`);
    clock.advanceBy(200);
    await Promise.resolve();
    expect(wakes().filter((w) => w.event.includes("entering summary"))).toHaveLength(1);
    clock.advanceBy(2_000 + 5_000);
    expect(wakes().filter((w) => w.event.includes("leaving summary"))).toHaveLength(1);
    expect(wakes().filter((w) => w.event.includes("entering summary"))).toHaveLength(1);
    center.dispose();
    registry.disposeAll();
  });
});

describe("MonitorRegistry reconnect output recovery", () => {
  it("delivers the re-hello replay once, trimming duplicate and overlapping UTF-8 ranges without backfill", async () => {
    const clock = new ManualClock();
    const delivered: string[] = [];
    let registry!: MonitorRegistry;
    let watchCount = 0;
    const before = `${"中\n".repeat(200)}${"字\n".repeat(50)}`; // exactly 1000 UTF-8 bytes
    const output = vi.fn(async (_id: string, cursor: number) => ({
      chunk: "",
      next_cursor: cursor,
      status: "running" as const,
      exit_code: null,
      total_size: 1008,
    }));
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_utf8", pid: 13 }),
      watch: async () => {
        watchCount++;
        if (watchCount === 2) {
          // The manager replays [1000, 1004) after re-hello. Duplicate it,
          // then send an overlapping [996, 1008) range as a safety check.
          registry.handleOutput("mon_utf8", "new\n", 1004);
          registry.handleOutput("mon_utf8", "new\n", 1004);
          registry.handleOutput("mon_utf8", "字\nnew\n中\n", 1008);
        }
      },
      output,
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = {
      notifyMonitorEvent: (_description: string, _taskId: string, text: string) => delivered.push(text),
      notify: () => {},
    } as unknown as NotifyCenter;
    registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "ticker", description: "ticker", persistent: true }, { cwd: "/tmp" } as ExtensionContext);
    registry.handleOutput("mon_utf8", before, 1000);
    await registry.rewatchAll();
    clock.advanceBy(200);
    const text = delivered.join("\n");
    expect(watchCount).toBe(2);
    expect(output).not.toHaveBeenCalled();
    expect(text).toBe(`${before.slice(0, -1)}\nnew\n中`);
    expect(text.match(/字/g)).toHaveLength(50);
    expect(text.match(/new/g)).toHaveLength(1);
    expect(text.match(/中/g)).toHaveLength(201);
    registry.disposeAll();
  });
});

describe("MonitorRegistry early exit (manual testing, 2026-09-24)", () => {
  function setup(opts: { list?: () => unknown[] } = {}) {
    const clock = new ManualClock();
    const sent: { details?: unknown }[] = [];
    const exited: string[] = [];
    let registry!: MonitorRegistry;
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      // `echo noop`: its line and exit share the socket read with the start
      // response, so they are dispatched before start() sees the task id.
      start: async () => {
        registry.handleOutput("mon_fast", "noop\n");
        registry.handleExit("mon_fast", { event: "task_exited", task_id: "mon_fast", exit_code: 0, duration_ms: 4 });
        return { task_id: "mon_fast", pid: 1 };
      },
      watch: async () => {},
      stop: async () => {},
      list: async () => opts.list?.() ?? [],
    } as unknown as ManagerClient;
    const center = new NotifyCenter({ sendMessage: (m) => sent.push(m), isIdle: () => true, clock });
    registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
      onExited: (id) => exited.push(id),
    });
    const wakes = () => sent.map((m) => m.details as FamulusWake | undefined).filter((d) => d?.kind === "monitor" || d?.kind === "monitor-terminal");
    return { clock, registry, center, exited, wakes };
  }

  it("an exit that beats registration still ends the monitor, with its output", async () => {
    const { clock, registry, center, exited, wakes } = setup();
    await registry.start({ command: "echo noop", description: "noop" }, { cwd: "/tmp" } as ExtensionContext);
    clock.advanceBy(1_000);
    center.settled(); // the run the "noop" event started is over; the exit notice follows it
    clock.advanceBy(TERMINAL_MS);
    expect(registry.has("mon_fast")).toBe(false);
    expect(registry.listActive()).toEqual([]);
    expect(exited).toEqual(["mon_fast"]);
    const statuses = wakes().map((w) => terminalStatus(w));
    expect(statuses).toContain("exited");
    expect(JSON.stringify(wakes())).toContain("noop");
    clock.advanceBy(400_000); // past the default timeout: no false "timed out"
    expect(wakes().map((w) => terminalStatus(w))).not.toContain("timeout");
    center.dispose();
    registry.disposeAll();
  });

  it("reconcile closes a monitor the manager reports as ended", async () => {
    const { registry, center, exited } = setup();
    (registry as unknown as { deps: { getClient: () => ManagerClient } }).deps.getClient().start = (async () => ({ task_id: "mon_fast", pid: 1 })) as never;
    await registry.start({ command: "tail -f x", description: "tail" }, { cwd: "/tmp" } as ExtensionContext);
    const record = { task_id: "mon_fast", session_id: "s", kind: "monitor", command: "tail -f x", cwd: "/tmp", pid: 1, status: "running", exit_code: null, signal: null, started_at: 0, ended_at: null, output_path: "/o", output_size: 0 };
    expect(registry.reconcile([record])).toEqual([]);
    expect(registry.has("mon_fast")).toBe(true);
    expect(registry.reconcile([{ ...record, status: "completed", exit_code: 0, ended_at: 10 }])).toEqual(["mon_fast"]);
    expect(registry.has("mon_fast")).toBe(false);
    expect(exited).toEqual(["mon_fast"]);
    center.dispose();
    registry.disposeAll();
  });

  it("a monitor that timed out after its exit was lost is settled in the index (the stuck '2 monitors')", async () => {
    const { WorkIndex } = await import("../../src/work-index");
    const { exitEventFromRecord } = await import("../../src/monitor");
    const clock = new ManualClock(0);
    const index = new WorkIndex({ clock });
    // task_started created the row; the exit event never arrived.
    index.upsert({ id: "mon_f87f7957", kind: "monitor", status: "running", title: "echo noop", startedAt: 0, countsAsWorker: false });
    index.upsert({ id: "mon_live", kind: "monitor", status: "running", title: "tail -f", startedAt: 0, countsAsWorker: false });
    expect(index.counts().monitors).toBe(2);
    const tasks = [
      { task_id: "mon_f87f7957", status: "completed", exit_code: 0, signal: null, started_at: 0, ended_at: 6, output_path: "/o", session_id: "s", kind: "monitor", command: "echo noop", cwd: "/", pid: 1, output_size: 5 },
      { task_id: "mon_live", status: "running", exit_code: null, signal: null, started_at: 0, ended_at: null, output_path: "/o2", session_id: "s", kind: "monitor", command: "tail -f", cwd: "/", pid: 2, output_size: 0 },
    ];
    const stale = index.staleLive(tasks);
    expect(stale.map((t) => t.task_id)).toEqual(["mon_f87f7957"]);
    for (const t of stale) index.patch(t.task_id, { status: "completed", exitCode: exitEventFromRecord(t).exit_code ?? null, endedAt: 6 });
    expect(index.counts().monitors).toBe(1);
    expect(index.staleLive(tasks)).toEqual([]);
  });
});

// Eval batch 1 (2026-09-29): a `tail -F | grep -m1` monitor exits right after
// its event. The exit arrived as a second wake, so the model answered, then
// spent a turn acknowledging "monitor exited" (every model, every run).
describe("MonitorRegistry exit right after an event", () => {
  async function setup() {
    const clock = new ManualClock();
    const sent: { details?: unknown; opts: { triggerTurn?: boolean; deliverAs?: string } }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_1", pid: 1 }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = new NotifyCenter({ sendMessage: (m, opts) => sent.push({ ...m, opts }), isIdle: () => true, clock });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "tail -F log | grep -m1 READY", description: "ready" }, { cwd: "/tmp" } as ExtensionContext);
    const exit = (code: number) =>
      registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: code, duration_ms: 10 });
    const turns = () => sent.filter((m) => m.opts.triggerTurn !== false);
    const exitMsg = () => sent.find((m) => terminalStatus(m.details) === "exited");
    return { center, clock, registry, exit, sent, turns, exitMsg };
  }

  // Batches 2–4: sent at once with triggerTurn: false while the model was
  // writing, the exit was appended at that turn's end, ahead of the event
  // steered in at the next turn's start (25 of 125 monitors).
  it("a clean exit inside the batch window: one wake, exit held until that run settles", async () => {
    const { center, clock, registry, exit, turns, exitMsg, sent } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    exit(0);
    expect(turns()).toHaveLength(1);
    expect(JSON.stringify(turns()[0].details)).toContain("READY token=AB12");
    expect(exitMsg()).toBeUndefined();
    center.settled();
    expect(exitMsg()).toBeUndefined();
    clock.advanceBy(TERMINAL_MS);
    expect(exitMsg()?.opts).toEqual({ triggerTurn: false });
    expect(sent.indexOf(exitMsg()!)).toBeGreaterThan(sent.indexOf(turns()[0]));
  });

  it("a clean exit shortly after a delivered event adds no turn, and follows the event", async () => {
    const { center, clock, registry, exit, turns, exitMsg } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    clock.advanceBy(300);
    clock.advanceBy(1_000);
    exit(0);
    expect(exitMsg()).toBeUndefined();
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(1);
    expect(exitMsg()?.opts).toEqual({ triggerTurn: false });
  });

  it("a clean exit after the event's run already settled is appended at once", async () => {
    const { center, clock, registry, exit, turns, exitMsg } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    clock.advanceBy(300);
    center.settled();
    clock.advanceBy(1_000);
    exit(0);
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(1);
    expect(exitMsg()?.opts).toEqual({ triggerTurn: false });
  });

  it("a failed exit still wakes", async () => {
    const { clock, registry, exit, turns, exitMsg } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    exit(1);
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(2);
    expect(exitMsg()?.opts.triggerTurn).toBe(true);
  });

  it("a clean exit long after the last event still wakes", async () => {
    const { clock, registry, exit, turns } = await setup();
    registry.handleOutput("mon_1", "tick\n");
    clock.advanceBy(10_000);
    exit(0);
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(2);
  });

  it("a clean exit with no event at all still wakes", async () => {
    const { clock, exit, turns } = await setup();
    exit(0);
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(1);
  });
});

// Eval batch 2: kimi-for-coding stopped its own monitor with task_stop; the
// lines still buffered and the SIGTERM exit then cost it two more turns.
describe("MonitorRegistry monitor stopped by the model", () => {
  it("delivers leftover lines and the exit without starting a turn", async () => {
    const clock = new ManualClock();
    const sent: { details?: unknown; opts: { triggerTurn?: boolean; deliverAs?: string } }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_1", pid: 1 }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = new NotifyCenter({ sendMessage: (m, opts) => sent.push({ ...m, opts }), isIdle: () => true, clock });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "tail -F out | grep .", description: "watch" }, { cwd: "/tmp" } as ExtensionContext);
    registry.handleOutput("mon_1", "compiling...\n");
    registry.noteStopRequested("mon_1");
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: null, signal: "SIGTERM", duration_ms: 10 });
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    expect(sent.filter((m) => m.opts.triggerTurn !== false)).toHaveLength(0);
    const statuses = sent.map((m) => terminalStatus(m.details) ?? "event");
    expect(statuses).toEqual(["event", "exited"]);
  });

  it("task_stop on a monitor tells the registry before stopping it", async () => {
    const order: string[] = [];
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      stop: async (id: string) => { order.push(`stop ${id}`); },
    } as unknown as ManagerClient;
    const tool = createTaskStopTool({ getClient: () => client, noteStopRequested: (id) => { order.push(`note ${id}`); } });
    await tool.execute("t", { task_id: "mon_1" }, undefined as never, undefined as never, {} as never);
    expect(order).toEqual(["note mon_1", "stop mon_1"]);
  });
});

// cubic review on #19 (2026-09-29/30).
describe("MonitorRegistry stop handling, from review", () => {
  async function setup(opts: { idle?: () => boolean; stop?: () => Promise<void> } = {}) {
    const clock = new ManualClock();
    const sent: { details?: unknown; opts: { triggerTurn?: boolean; deliverAs?: string } }[] = [];
    const stops: string[] = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_1", pid: 1 }),
      watch: async () => {},
      stop: opts.stop ?? (async (_id: string, reason: string) => { stops.push(reason); }),
    } as unknown as ManagerClient;
    const center = new NotifyCenter({ sendMessage: (m, o) => sent.push({ ...m, opts: o }), isIdle: opts.idle ?? (() => true), clock });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "tail -F out | grep --line-buffered .", description: "watch", persistent: true }, { cwd: "/tmp" } as ExtensionContext);
    const turns = () => sent.filter((m) => m.opts.triggerTurn !== false);
    const statuses = () => sent.map((m) => terminalStatus(m.details) ?? "event");
    return { clock, center, registry, sent, stops, turns, statuses };
  }

  // The earlier test ran with an idle agent and nothing pending, so nothing
  // was ever held. Here the model is mid-run: an event is coalesced, the
  // model stops the monitor, more lines are still buffered, then it exits.
  it("a busy agent that stops its monitor gets no turn, one coalesced event, then the exit", async () => {
    let idle = false;
    const { clock, center, registry, turns, statuses, sent } = await setup({ idle: () => idle });
    registry.handleOutput("mon_1", "line a\n");
    clock.advanceBy(300);
    registry.noteStopRequested("mon_1");
    registry.handleOutput("mon_1", "line b\n");
    clock.advanceBy(300);
    registry.handleOutput("mon_1", "line c\n");
    clock.advanceBy(300);
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: null, signal: "SIGTERM", duration_ms: 10 });
    idle = true;
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    expect(turns()).toHaveLength(0);
    expect(statuses()).toEqual(["event", "exited"]);
    expect(JSON.stringify(sent[0].details)).toContain("line c");
  });

  it("a stop that fails leaves the monitor waking the model", async () => {
    const { clock, registry, turns } = await setup();
    const undo = registry.noteStopRequested("mon_1");
    undo?.();
    registry.handleOutput("mon_1", "line a\n");
    clock.advanceBy(300);
    expect(turns()).toHaveLength(1);
  });

  it("task_stop rolls the note back when the stop fails", async () => {
    const order: string[] = [];
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      stop: async () => { throw new Error("manager gone"); },
    } as unknown as ManagerClient;
    const tool = createTaskStopTool({
      getClient: () => client,
      noteStopRequested: (id) => { order.push(`note ${id}`); return () => order.push(`undo ${id}`); },
    });
    await expect(tool.execute("t", { task_id: "mon_1" }, undefined as never, undefined as never, {} as never)).rejects.toThrow();
    expect(order).toEqual(["note mon_1", "undo mon_1"]);
  });

  it("output dropped after the model stopped the monitor does not trigger the rate-limit stop", async () => {
    const { clock, registry, stops, turns } = await setup();
    registry.noteStopRequested("mon_1");
    for (let i = 0; i < 151; i++) {
      registry.handleOutput("mon_1", `line ${i}\n`);
      clock.advanceBy(200);
      await Promise.resolve();
    }
    expect(stops).not.toContain("rate-limit");
    expect(turns()).toHaveLength(0);
  });

  it.each(["stopped:tui", "stopped:cli"])("a clean exit right after an event, but %s, still wakes", async (reason) => {
    const { center, clock, registry, turns, statuses } = await setup();
    registry.handleOutput("mon_1", "line a\n");
    clock.advanceBy(300);
    center.settled();
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: 0, end_reason: reason, duration_ms: 10 });
    clock.advanceBy(TERMINAL_MS);
    expect(statuses()).toEqual(["event", "exited"]);
    expect(turns()).toHaveLength(2);
  });
});

describe("MonitorRegistry stores deadline and last output", () => {
  async function start(params: { timeout_ms?: number; persistent?: boolean }) {
    const clock = new ManualClock(1_000);
    const sent: { details?: unknown; content?: string }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_1", pid: 1 }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = new NotifyCenter({
      sendMessage: (message) => sent.push(message),
      isIdle: () => true,
      clock,
    });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "tail -F x", description: "watch", ...params }, { cwd: "/tmp" } as ExtensionContext);
    return { clock, registry, center, sent };
  }

  it("stores a deadline for timed monitors and marks persistent ones", async () => {
    const clock = new ManualClock(1_000);
    let nextId = 1;
    const manager = {
      ensureAvailable: async () => true,
      start: async () => ({ task_id: `mon_${nextId++}`, pid: nextId }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => null,
      trackTask: () => {},
      clock,
    });
    await registry.start(
      { command: "tick", description: "ticker", timeout_ms: 120_000 },
      { cwd: "/tmp" } as ExtensionContext,
    );
    await registry.start(
      { command: "watch", description: "watcher", persistent: true },
      { cwd: "/tmp" } as ExtensionContext,
    );
    clock.advanceBy(30_000);
    const active = registry.listActive();
    expect(active).toEqual([
      expect.objectContaining({
        taskId: "mon_1",
        description: "ticker",
        remainingMs: 90_000,
        persistent: false,
      }),
      expect.objectContaining({
        taskId: "mon_2",
        description: "watcher",
        remainingMs: null,
        persistent: true,
      }),
    ]);
    registry.disposeAll();
  });

  it("stores remainingMs, persistent, and lastEventAt after output", async () => {
    const { clock, registry, center } = await start({ timeout_ms: 60_000 });
    expect(registry.listActive()).toEqual([
      expect.objectContaining({
        taskId: "mon_1",
        description: "watch",
        startedAt: 1_000,
        remainingMs: 60_000,
        persistent: false,
      }),
    ]);
    expect(registry.listActive()[0].lastEventAt).toBeUndefined();
    expect(registry.listActive()[0].lastOutputAt).toBeUndefined();
    registry.handleOutput("mon_1", "tick\n");
    expect(registry.listActive()[0].lastOutputAt).toBe(1_000);
    expect(registry.listActive()[0].lastEventAt).toBeUndefined();
    clock.advanceBy(200);
    expect(registry.listActive()[0].lastEventAt).toBe(1_200);
    expect(registry.listActive()[0].lastOutputAt).toBe(1_000);
    expect(registry.listActive()[0].remainingMs).toBe(59_800);
    expect(registry.listActive()[0].persistent).toBe(false);
    center.dispose();
    registry.disposeAll();
  });

  it("updates lastOutputAt on deduped line records without changing lastEventAt", async () => {
    const { clock, registry, center } = await start({ persistent: true });
    const line = (text: string, end: number, at: number): import("../../src/manager-client").MonitorOutputRecord => ({
      version: 1, kind: "line", observed_at: at, cursor_start: end - Buffer.byteLength(text + "\n"), cursor_end: end,
      preview: text, truncated: false, fingerprint: `blake3:v1:${text}`, timestamp_fingerprint: `blake3:iso-edge-v1:${text}`,
    });
    registry.handleOutput("mon_1", "fail\n", 5, [line("fail", 5, 1)]);
    clock.advanceBy(200);
    expect(registry.listActive()[0].lastEventAt).toBe(1_200);
    expect(registry.listActive()[0].lastOutputAt).toBe(1_000);
    clock.advanceBy(300);
    registry.handleOutput("mon_1", "fail\n", 10, [line("fail", 10, 2)]);
    expect(registry.listActive()[0].lastOutputAt).toBe(1_500);
    expect(registry.listActive()[0].lastEventAt).toBe(1_200);
    center.dispose();
    registry.disposeAll();
  });

  it("does not set lastOutputAt on a raw fragment without a newline", async () => {
    const { registry, center } = await start({ persistent: true });
    registry.handleOutput("mon_1", "partial");
    expect(registry.listActive()[0].lastOutputAt).toBeUndefined();
    registry.handleOutput("mon_1", "done\n");
    expect(registry.listActive()[0].lastOutputAt).toBe(1_000);
    center.dispose();
    registry.disposeAll();
  });

  it("stores remainingMs null and persistent true", async () => {
    const { registry, center } = await start({ persistent: true });
    expect(registry.listActive()[0].remainingMs).toBeNull();
    expect(registry.listActive()[0].persistent).toBe(true);
    expect(registry.listActive()[0].lastOutputAt).toBeUndefined();
    center.dispose();
    registry.disposeAll();
  });

  it("timeout notice reports an unconfirmed exit after the terminal window", async () => {
    const { clock, registry, center, sent } = await start({ timeout_ms: 5_000 });
    clock.advanceBy(5_000);
    await Promise.resolve();
    await Promise.resolve();
    const beforeWindow = sent.filter((entry) => (entry.details as { kind?: string } | undefined)?.kind === "monitor-terminal");
    expect(beforeWindow).toHaveLength(0);
    clock.advanceBy(10_000);
    const timeout = sent.find((entry) => (entry.details as { kind?: string } | undefined)?.kind === "monitor-terminal");
    expect(timeout?.content).toMatch(/Timeout elapsed\. Stop requested; exit unconfirmed/);
    expect(timeout?.content).not.toMatch(/re-arm if needed/);
    expect(JSON.stringify(timeout?.details)).not.toMatch(/re-arm if needed/);
    center.dispose();
    registry.disposeAll();
  });
});
