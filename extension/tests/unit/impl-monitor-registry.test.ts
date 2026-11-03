import { describe, expect, it, vi } from "vitest";
import { ManualClock } from "../../src/clock";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import { createTaskStopTool } from "../../src/task-tools";
import type { FamulusWake } from "../../src/wake";

describe("MonitorRegistry saturation", () => {
  it("stops on sustained drops even when accepted batches interrupt them", async () => {
    const clock = new ManualClock();
    const stopped: string[] = [];
    const sent: { details?: unknown }[] = [];
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      start: async () => ({ task_id: "mon_1", pid: 123 }),
      watch: async () => {},
      stop: async (taskId: string) => { stopped.push(taskId); },
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
    });

    const { taskId } = await registry.start(
      { command: "ticker", description: "ticker", persistent: true },
      { cwd: "/tmp" } as ExtensionContext,
    );
    for (let i = 0; i < 151; i++) {
      registry.handleOutput(taskId, `line ${i}\n`);
      clock.advanceBy(200);
      await Promise.resolve();
      await Promise.resolve();
    }

    expect(stopped).toEqual(["mon_1"]);
    expect(registry.has("mon_1")).toBe(false);
    const stopWake = sent.map((item) => item.details as FamulusWake | undefined).find(
      (details): details is Extract<FamulusWake, { kind: "monitor" }> =>
        details?.kind === "monitor" && details.status === "stopped",
    );
    expect(stopWake?.droppedLines).toBeGreaterThan(0);
    expect(events.some((event) => event.type === "monitor.drop" && event.fields?.id === "mon_1")).toBe(true);
    expect(events).toContainEqual({ type: "monitor.stop", fields: { id: "mon_1", reason: "rate-limit" } });
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
    const wakes = () => sent.map((m) => m.details as FamulusWake | undefined).filter((d) => d?.kind === "monitor");
    return { clock, registry, center, exited, wakes };
  }

  it("an exit that beats registration still ends the monitor, with its output", async () => {
    const { clock, registry, center, exited, wakes } = setup();
    await registry.start({ command: "echo noop", description: "noop" }, { cwd: "/tmp" } as ExtensionContext);
    clock.advanceBy(1_000);
    center.settled(); // the run the "noop" event started is over; the exit notice follows it
    expect(registry.has("mon_fast")).toBe(false);
    expect(registry.listActive()).toEqual([]);
    expect(exited).toEqual(["mon_fast"]);
    const statuses = wakes().map((w) => (w as { status?: string }).status);
    expect(statuses).toContain("exited");
    expect(JSON.stringify(wakes())).toContain("noop");
    clock.advanceBy(400_000); // past the default timeout: no false "timed out"
    expect(wakes().map((w) => (w as { status?: string }).status)).not.toContain("timeout");
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
    const exitMsg = () => sent.find((m) => (m.details as { status?: string }).status === "exited");
    return { center, clock, registry, exit, sent, turns, exitMsg };
  }

  // Batches 2–4: sent at once with triggerTurn: false while the model was
  // writing, the exit was appended at that turn's end, ahead of the event
  // steered in at the next turn's start (25 of 125 monitors).
  it("a clean exit inside the batch window: one wake, exit held until that run settles", async () => {
    const { center, registry, exit, turns, exitMsg, sent } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    exit(0);
    expect(turns()).toHaveLength(1);
    expect(JSON.stringify(turns()[0].details)).toContain("READY token=AB12");
    expect(exitMsg()).toBeUndefined();
    center.settled();
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
    expect(turns()).toHaveLength(1);
    expect(exitMsg()?.opts).toEqual({ triggerTurn: false });
  });

  it("a failed exit still wakes", async () => {
    const { registry, exit, turns, exitMsg } = await setup();
    registry.handleOutput("mon_1", "READY token=AB12\n");
    exit(1);
    expect(turns()).toHaveLength(2);
    expect(exitMsg()?.opts.triggerTurn).toBe(true);
  });

  it("a clean exit long after the last event still wakes", async () => {
    const { clock, registry, exit, turns } = await setup();
    registry.handleOutput("mon_1", "tick\n");
    clock.advanceBy(10_000);
    exit(0);
    expect(turns()).toHaveLength(2);
  });

  it("a clean exit with no event at all still wakes", async () => {
    const { exit, turns } = await setup();
    exit(0);
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
    expect(sent.filter((m) => m.opts.triggerTurn !== false)).toHaveLength(0);
    const statuses = sent.map((m) => (m.details as { status?: string }).status ?? "event");
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
    const statuses = () => sent.map((m) => (m.details as { status?: string }).status ?? "event");
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

  it("a clean exit right after an event, but stopped from the TUI, still wakes", async () => {
    const { center, clock, registry, turns, statuses } = await setup();
    registry.handleOutput("mon_1", "line a\n");
    clock.advanceBy(300);
    center.settled();
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: 0, end_reason: "tui", duration_ms: 10 });
    expect(statuses()).toEqual(["event", "exited"]);
    expect(turns()).toHaveLength(2);
  });
});

describe("MonitorRegistry running-monitor remaining time", () => {
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
});
