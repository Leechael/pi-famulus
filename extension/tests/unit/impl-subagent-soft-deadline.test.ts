/**
 * Soft deadline (design.md §4.6 child lifecycle, decisions/subagent-soft-deadline.md):
 * reaching timeoutMs wakes the parent and never aborts the child; only the
 * opt-in hardTimeoutMs aborts.
 */
import { beforeEach, describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { ChildShellTracker, createOverrunNotifier, type OverrunTick } from "../../src/subagent/overrun";
import { SubagentRegistry } from "../../src/subagent/registry";
import type { FormattedWake } from "../../src/wake";
import { InProcessRunner, type InProcessRunnerOptions } from "../../src/subagent/runner";
import type { ChildRunRequest } from "../../src/subagent/types";
import { SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

function makeReq(overrides: Partial<ChildRunRequest> = {}): ChildRunRequest {
  return {
    childId: "ch_soft",
    runId: "run_soft",
    name: "worker-1",
    prompt: "do the thing",
    agent: WORKER_AGENT,
    timeoutMs: 1_000,
    depth: 1,
    ...overrides,
  };
}

describe("soft deadline", () => {
  let clock: ManualClock;
  let factory: SessionFactory;
  let ticks: OverrunTick[];
  const runner = (opts: Partial<InProcessRunnerOptions> = {}) =>
    new InProcessRunner({
      createSession: factory.fn,
      clock,
      stallMs: 0, // stall watchdog off unless a test turns it on
      overrunRepeatMs: 300,
      onOverrun: (t) => ticks.push(t),
      ...opts,
    });

  beforeEach(() => {
    clock = new ManualClock();
    factory = new SessionFactory();
    factory.autoComplete = null;
    ticks = [];
  });

  it("wakes the parent at the budget and does not abort the child or its shell", async () => {
    const handle = await runner().start(makeReq());
    const tool = factory.sessions[0].runTool();
    clock.advanceBy(999);
    expect(ticks).toHaveLength(0);
    clock.advanceBy(1);
    expect(ticks).toEqual([
      {
        childId: "ch_soft",
        elapsedMs: 1_000,
        budgetMs: 1_000,
        reminder: 1,
        nextReminderMs: 300,
        lastEventAt: 0,
        hardRemainingMs: null,
      },
    ]);
    expect(handle.status()).toBe("running");
    expect(factory.sessions[0].aborts).toBe(0);
    expect(tool.signal.aborted).toBe(false);
    tool.end();
    factory.sessions[0].complete("finished late");
    const result = await handle.result;
    expect(result).toMatchObject({ status: "completed", text: "finished late" });
    expect(result.error).toBeUndefined();
  });

  it("clamps a sub-millisecond overrunRepeatMs to 1ms so reminders do not storm", async () => {
    const handle = await runner({ overrunRepeatMs: 0.5 }).start(makeReq());
    clock.advanceBy(1_000);
    expect(ticks).toHaveLength(1);
    expect(ticks[0].nextReminderMs).toBe(1);
    clock.advanceBy(5);
    expect(ticks).toHaveLength(6);
    expect(handle.status()).toBe("running");
  });

  it("repeats every overrunRepeatMs while the child runs, numbering the reminders", async () => {
    const handle = await runner().start(makeReq());
    clock.advanceBy(1_000 + 300 * 3);
    expect(ticks.map((t) => [t.reminder, t.elapsedMs])).toEqual([
      [1, 1_000],
      [2, 1_300],
      [3, 1_600],
      [4, 1_900],
    ]);
    expect(handle.status()).toBe("running");
  });

  it("sends no reminder after the child settles, and leaves no timer armed", async () => {
    const handle = await runner().start(makeReq());
    clock.advanceBy(1_000);
    factory.sessions[0].complete("done");
    await handle.result;
    expect(clock.pendingTimers).toBe(0);
    clock.advanceBy(10_000);
    expect(ticks).toHaveLength(1);
  });

  it("sends no reminder after interrupt", async () => {
    const handle = await runner().start(makeReq());
    clock.advanceBy(1_000);
    await handle.interrupt();
    expect(clock.pendingTimers).toBe(0);
    clock.advanceBy(10_000);
    expect(ticks).toHaveLength(1);
    expect((await handle.result).status).toBe("interrupted");
  });

  it("an observer that throws does not break the schedule", async () => {
    const handle = await runner({
      onOverrun: (t) => {
        ticks.push(t);
        throw new Error("observer bug");
      },
    }).start(makeReq());
    clock.advanceBy(1_300);
    expect(ticks).toHaveLength(2);
    expect(handle.status()).toBe("running");
  });

  describe("extend", () => {
    it("re-arms the deadline at now + ms; the reminder count continues", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(1_000); // reminder 1
      handle.extend(2_000); // new deadline t=3000
      clock.advanceBy(1_999);
      expect(ticks).toHaveLength(1); // the 300ms repeat no longer applies
      clock.advanceBy(1);
      expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 3_000, budgetMs: 3_000 });
      clock.advanceBy(300);
      expect(ticks[2]).toMatchObject({ reminder: 3, elapsedMs: 3_300 });
    });

    it("before the first reminder moves the first deadline", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(500);
      handle.extend(5_000); // deadline t=5500, not t=1000
      clock.advanceBy(4_999);
      expect(ticks).toHaveLength(0);
      clock.advanceBy(1);
      expect(ticks[0]).toMatchObject({ reminder: 1, elapsedMs: 5_500, budgetMs: 5_500 });
    });

    it("throws for a child that is not running", async () => {
      const handle = await runner().start(makeReq());
      factory.sessions[0].complete("done");
      await handle.result;
      expect(() => handle.extend(1_000)).toThrow(/not running/);
    });
  });

  describe("steer after a reminder", () => {
    it("postpones the next reminder; steer before the deadline moves nothing", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(500);
      await handle.steer("early hint");
      clock.advanceBy(500);
      expect(ticks).toHaveLength(1); // deadline unchanged at t=1000
      clock.advanceBy(200); // t=1200
      await handle.steer("wrap up");
      clock.advanceBy(299); // t=1499: the original t=1300 reminder was postponed
      expect(ticks).toHaveLength(1);
      clock.advanceBy(1); // t=1500 = steer + 300
      expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 1_500 });
    });

    it("followUp counts as the parent acting too", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(1_100);
      await handle.followUp("queued note");
      clock.advanceBy(299); // t=1399
      expect(ticks).toHaveLength(1);
      clock.advanceBy(1);
      expect(ticks).toHaveLength(2);
    });

    // Review P2 (PR #34): a delivery that resolves after its turn ended must
    // not move a later turn's schedule.
    for (const via of ["steer", "followUp"] as const) {
      it(`a ${via} still in flight across resume does not shift the new turn's reminders`, async () => {
        const handle = await runner().start(makeReq());
        const session = factory.sessions[0];
        clock.advanceBy(1_000); // reminder 1 of turn 1
        session.deliveryGateOpen = false;
        const late = handle[via]("old instruction");
        clock.advanceBy(100);
        session.complete("done");
        await handle.result;
        await handle.resume("next turn", { timeoutMs: 5_000 }); // t=1100, deadline 6100
        ticks = [];
        clock.advanceBy(5_000); // reminder 1 of turn 2 at 6100, next due 6400
        expect(ticks).toHaveLength(1);
        clock.advanceBy(100); // t=6200: the old delivery lands now
        session.openDeliveryGate();
        await late;
        clock.advanceBy(199); // t=6399
        expect(ticks).toHaveLength(1);
        clock.advanceBy(1); // t=6400: on the turn-2 schedule, not 6200 + 300
        expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 5_300 });
      });

      it(`a ${via} still in flight when the child settles changes nothing`, async () => {
        const handle = await runner().start(makeReq());
        const session = factory.sessions[0];
        clock.advanceBy(1_000);
        session.deliveryGateOpen = false;
        const late = handle[via]("old instruction");
        session.complete("done");
        await handle.result;
        session.openDeliveryGate();
        await late;
        expect(clock.pendingTimers).toBe(0);
        clock.advanceBy(10_000);
        expect(ticks).toHaveLength(1);
      });
    }

    // Review P2 (PR #34): a steer right after extend must not pull the next
    // reminder in front of the extended deadline ("past budget" while inside it).
    for (const via of ["steer", "followUp"] as const) {
      for (const [label, ext] of [["long", 2_000], ["short", 200]] as const) {
        it(`${via} right after a ${label} extend keeps the extended deadline`, async () => {
          const handle = await runner().start(makeReq());
          clock.advanceBy(1_000); // reminder 1 at t=1000
          handle.extend(ext); // deadline t=1000+ext
          await handle[via]("carry on");
          clock.advanceBy(ext - 1);
          expect(ticks).toHaveLength(1);
          clock.advanceBy(1);
          expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 1_000 + ext, budgetMs: 1_000 + ext });
          // Past the extended deadline, a steer postpones the repeat as before.
          clock.advanceBy(100);
          await handle[via]("wrap up");
          clock.advanceBy(299);
          expect(ticks).toHaveLength(2);
          clock.advanceBy(1);
          expect(ticks[2]).toMatchObject({ reminder: 3, elapsedMs: 1_000 + ext + 400 });
        });
      }
    }
  });

  // Owner decision on PR #34: while the child waits on need_decision the
  // parent already holds a supervisor-request wake for it. Reminders are
  // held until the decision resolves; the budget is not reset.
  describe("pending need_decision", () => {
    type Pausable = { pauseStall(): void; resumeStall(): void };

    it("holds the first reminder until the decision resolves, then sends it at once", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(800);
      (handle as unknown as Pausable).pauseStall(); // contact_supervisor need_decision
      clock.advanceBy(2_000); // deadline (1000) and several repeats pass
      expect(ticks).toHaveLength(0);
      expect(handle.status()).toBe("running");
      (handle as unknown as Pausable).resumeStall(); // parent replied at t=2800
      clock.advanceBy(0);
      expect(ticks).toEqual([expect.objectContaining({ reminder: 1, elapsedMs: 2_800, budgetMs: 1_000 })]);
      clock.advanceBy(300);
      expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 3_100 });
    });

    it("holds repeats too, and does not restart the budget", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(1_000); // reminder 1
      (handle as unknown as Pausable).pauseStall();
      clock.advanceBy(10_000);
      expect(ticks).toHaveLength(1);
      (handle as unknown as Pausable).resumeStall();
      clock.advanceBy(0);
      expect(ticks[1]).toMatchObject({ reminder: 2, elapsedMs: 11_000, budgetMs: 1_000 });
    });

    it("a decision that resolves inside the budget changes nothing", async () => {
      const handle = await runner().start(makeReq());
      (handle as unknown as Pausable).pauseStall();
      clock.advanceBy(500);
      (handle as unknown as Pausable).resumeStall();
      clock.advanceBy(499);
      expect(ticks).toHaveLength(0);
      clock.advanceBy(1);
      expect(ticks[0]).toMatchObject({ reminder: 1, elapsedMs: 1_000 });
    });

    it("the hard ceiling still fires while a decision is pending", async () => {
      const handle = await runner({ hardTimeoutMs: 2_000 }).start(makeReq());
      (handle as unknown as Pausable).pauseStall();
      clock.advanceBy(2_000);
      expect(await handle.result).toMatchObject({ status: "interrupted", error: "timeout" });
    });
  });

  describe("hard ceiling (opt-in hardTimeoutMs)", () => {
    it("is off by default: hours past the budget the child still runs", async () => {
      const handle = await runner().start(makeReq());
      const tool = factory.sessions[0].runTool();
      clock.advanceBy(6 * 3_600_000);
      expect(handle.status()).toBe("running");
      expect(tool.signal.aborted).toBe(false);
      expect(factory.sessions[0].aborts).toBe(0);
    });

    it("when set, aborts at the ceiling, stops the shell, and settles interrupted (timeout)", async () => {
      const handle = await runner({ hardTimeoutMs: 2_000 }).start(makeReq());
      const tool = factory.sessions[0].runTool();
      clock.advanceBy(1_999);
      expect(ticks.length).toBeGreaterThan(0); // soft wakes came first
      expect(tool.signal.aborted).toBe(false);
      clock.advanceBy(1);
      const result = await handle.result;
      expect(result).toMatchObject({ status: "interrupted", error: "timeout" });
      await tick();
      expect(tool.signal.aborted).toBe(true);
      expect(factory.sessions[0].aborts).toBe(1);
      expect(clock.pendingTimers).toBe(0);
    });

    it("extend does not move the hard ceiling", async () => {
      const handle = await runner({ hardTimeoutMs: 2_000 }).start(makeReq());
      clock.advanceBy(1_000);
      handle.extend(60_000);
      clock.advanceBy(1_000);
      expect((await handle.result).error).toBe("timeout");
    });
  });

  describe("resume", () => {
    it("honours a new turn budget and restarts the reminder count", async () => {
      const handle = await runner().start(makeReq());
      clock.advanceBy(1_000);
      factory.sessions[0].complete("first");
      await handle.result;
      ticks = [];
      await handle.resume("continue", { timeoutMs: 5_000 });
      clock.advanceBy(4_999);
      expect(ticks).toHaveLength(0);
      clock.advanceBy(1);
      expect(ticks[0]).toMatchObject({ reminder: 1, elapsedMs: 5_000, budgetMs: 5_000 });
    });

    it("without timeoutMs uses the spawn budget", async () => {
      const handle = await runner().start(makeReq());
      factory.sessions[0].complete("first");
      await handle.result;
      await handle.resume("continue");
      clock.advanceBy(1_000);
      expect(ticks[0]).toMatchObject({ reminder: 1, budgetMs: 1_000 });
    });
  });

  describe("stall interplay", () => {
    it("a stall retry does not restart the budget", async () => {
      const handle = await runner({ stallMs: 500, stallRetryDelayMs: 100 }).start(makeReq({ timeoutMs: 2_000 }));
      clock.advanceBy(500); // stall
      clock.advanceBy(100); // retry generation at t=600
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(2);
      for (let i = 0; i < 3; i++) {
        clock.advanceBy(400);
        factory.sessions[0].event();
      } // t=1800
      clock.advanceBy(199);
      expect(ticks).toHaveLength(0);
      clock.advanceBy(1); // t=2000: the turn budget, not 600 + 2000
      expect(ticks[0]).toMatchObject({ reminder: 1, elapsedMs: 2_000 });
      expect(handle.status()).toBe("running");
    });

    it("a deadline inside the retry delay wakes the parent and the retry still runs", async () => {
      const handle = await runner({ stallMs: 500, stallRetryDelayMs: 5_000 }).start(makeReq({ timeoutMs: 800 }));
      clock.advanceBy(500); // stall at t=500; retry due t=5500
      clock.advanceBy(300); // t=800: soft deadline mid-delay
      expect(ticks).toHaveLength(1);
      expect(handle.status()).toBe("running");
      clock.advanceBy(4_700); // t=5500: retry
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(2);
      factory.sessions[0].complete("recovered");
      expect(await handle.result).toMatchObject({ status: "completed", stalls: 1 });
    });

    it("the reminder schedule survives a retry (no duplicate, no reset)", async () => {
      const handle = await runner({ stallMs: 500, stallRetryDelayMs: 100 }).start(makeReq({ timeoutMs: 400 }));
      clock.advanceBy(400); // reminder 1 at t=400, next due t=700
      clock.advanceBy(100); // stall at t=500
      clock.advanceBy(100); // retry at t=600
      await tick();
      clock.advanceBy(100); // t=700
      expect(ticks.map((t) => t.reminder)).toEqual([1, 2]);
      expect(ticks[1].elapsedMs).toBe(700);
      expect(handle.status()).toBe("running");
    });
  });
});

describe("soft deadline, as the parent sees it (registry + runner + overrun notifier)", () => {
  function stack(hardTimeoutMs = 0) {
    const clock = new ManualClock();
    const registry = new SubagentRegistry({ clock });
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const shells = new ChildShellTracker();
    const wakes: FormattedWake[] = [];
    const events: { type: string; fields: Record<string, unknown> }[] = [];
    const runner = new InProcessRunner({
      createSession: factory.fn,
      clock,
      stallMs: 5 * 60_000,
      overrunRepeatMs: 10 * 60_000,
      hardTimeoutMs,
      acquire: (req) => registry.admitChild(req.childId),
      onOverrun: createOverrunNotifier({
        now: () => clock.now(),
        registry,
        shells,
        stat: () => ({ size: 4096, mtimeMs: clock.now() - 2_000 }),
        notify: (wake) => wakes.push(wake),
        logEvent: (type, fields) => events.push({ type, fields }),
      }),
    });
    registry.setRunner(runner);
    const run = registry.createRun("tasks");
    const childId = registry.addChild(run.runId, { name: "tester", agent: "worker" });
    const req: ChildRunRequest = { ...makeReq(), childId, runId: run.runId, name: "tester", timeoutMs: 30 * 60_000 };
    return { clock, registry, factory, shells, wakes, events, req, runId: run.runId };
  }

  it("a child blocked on a long test run: the parent is woken, the child and shell keep running", async () => {
    const { clock, registry, factory, shells, wakes, events, req, runId } = stack();
    const handle = await registry.startChild(req);
    const tool = factory.sessions[0].runTool(); // child bash: npm test
    shells.start(req.childId, { taskId: "sh_t", command: "npm test", startedAt: clock.now(), outputPath: "/o/sh_t.output" });
    clock.advanceBy(30 * 60_000);
    expect(wakes).toHaveLength(1);
    expect(wakes[0].details).toMatchObject({
      kind: "subagent-overrun",
      runId,
      childId: req.childId,
      name: "tester",
      reminder: 1,
      shell: { taskId: "sh_t", command: "npm test", outputBytes: 4096, growing: true },
    });
    expect(events).toEqual([
      {
        type: "agent.overrun",
        fields: {
          child_id: req.childId,
          run_id: runId,
          reminder: 1,
          elapsed_ms: 30 * 60_000,
          budget_ms: 30 * 60_000,
          shell_task_id: "sh_t",
          shell_elapsed_ms: 30 * 60_000,
          output_bytes: 4096,
          growing: true,
        },
      },
    ]);
    expect(registry.get(runId)!.children[0].status).toBe("running");
    expect(tool.signal.aborted).toBe(false);
    clock.advanceBy(10 * 60_000);
    expect(wakes.map((w) => (w.details as { reminder: number }).reminder)).toEqual([1, 2]);
    tool.end();
    shells.end(req.childId, "sh_t");
    factory.sessions[0].complete("all green");
    expect(await handle.result).toMatchObject({ status: "completed", text: "all green" });
    clock.advanceBy(60 * 60_000);
    expect(wakes).toHaveLength(2); // nothing after settle
  });

  it("a disposed run gets no overrun wake", async () => {
    const { clock, registry, wakes, req, runId } = stack();
    await registry.startChild(req);
    registry.disposeRun(runId);
    clock.advanceBy(60 * 60_000);
    expect(wakes).toHaveLength(0);
  });

  it("the queue wait does not count toward the soft budget", async () => {
    let release: (() => void) | null = null;
    const clock = new ManualClock();
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const ticks: OverrunTick[] = [];
    const runner = new InProcessRunner({
      createSession: factory.fn,
      clock,
      stallMs: 0,
      onOverrun: (t) => ticks.push(t),
      acquire: () => new Promise<() => void>((resolve) => (release = () => resolve(() => {}))),
    });
    const started = runner.start(makeReq({ timeoutMs: 1_000 }));
    await tick();
    clock.advanceBy(5_000); // queued for five budgets
    release!();
    await started;
    clock.advanceBy(999);
    expect(ticks).toHaveLength(0);
    clock.advanceBy(1);
    expect(ticks[0]).toMatchObject({ elapsedMs: 1_000 });
  });

  it("with a hard ceiling the parent still sees the overrun first, then the child settles interrupted", async () => {
    const { clock, registry, factory, wakes, req, runId } = stack(45 * 60_000);
    const handle = await registry.startChild(req);
    // Keep a tool in flight so the 5-minute stall watchdog stays paused;
    // an idle child would stall long before the 45-minute ceiling.
    factory.sessions[0].runTool();
    clock.advanceBy(45 * 60_000); // reminders at 30 and 40 min, ceiling at 45
    expect(wakes.map((w) => (w.details as { reminder: number }).reminder)).toEqual([1, 2]);
    // Review P2 (PR #34): the wake must not promise the child keeps running.
    expect(wakes.map((w) => (w.details as { hardCeilingMs?: number }).hardCeilingMs)).toEqual([15 * 60_000, 5 * 60_000]);
    expect(wakes[1].content).toContain("the configured hard ceiling stops it in 5m");
    expect(await handle.result).toMatchObject({ status: "interrupted", error: "timeout" });
    await tick();
    expect(registry.get(runId)!.children[0].status).toBe("interrupted");
  });
});
