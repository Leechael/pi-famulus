/**
 * Each wake that starts a parent turn re-reads the whole
 * parent context. Three notices should not cost a turn, or not without
 * limit:
 *
 * - a child's progress_update is information; delivering it must not
 *   start a parent turn.
 * - overrun reminders for a child past its soft budget back off and
 *   stop after a bounded number. Today they repeat every 10 minutes for as
 *   long as the child runs: 34 wakes for a child that runs 6 hours on the
 *   default 30-minute budget.
 * - a monitor that printed nothing and timed out must not start a turn.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import type { ManagerClient } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import { InProcessRunner } from "../../src/subagent/runner";
import { resetHostedFakes, settleAsync, startHostedExtension, until } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";
import { SessionFactory, WORKER_AGENT } from "./subagent-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-wake-cost-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

const MIN = 60_000;
const HOUR = 60 * MIN;

describe("a child's progress_update", () => {
  it.fails("reaches the idle parent without starting a turn", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-progress", cwd: home });
    try {
      const childId = await ext.spawnChild();
      const before = ext.pi.sent.length;
      ext.manager.emit({
        event: "agent_comms",
        child_id: childId,
        request_id: "rq_progress",
        reason: "progress_update",
        message: "Halfway: 12 of 24 files migrated.",
        name: "worker",
      });
      await settleAsync();
      await until(() => ext.pi.sent.length > before, "the progress notice");
      const notice = ext.pi.sent.slice(before).find((entry) => (entry.message.content ?? "").includes("12 of 24 files"));
      expect({ delivered: Boolean(notice), startsTurn: notice?.options?.triggerTurn === true }).toEqual({
        delivered: true,
        startsTurn: false,
      });
    } finally {
      await ext.shutdown();
    }
  });
});

describe("overrun reminders for a long child", () => {
  it.fails("back off and stop after a bounded number", async () => {
    const clock = new ManualClock();
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const at: number[] = [];
    // Production defaults: 30-minute soft budget, reminder interval left to the runner.
    const runner = new InProcessRunner({ createSession: factory.fn, clock, stallMs: 0, onOverrun: () => at.push(clock.now()) });
    const handle = await runner.start({
      childId: "ch_long",
      runId: "run_long",
      name: "long",
      prompt: "run the soak test",
      agent: WORKER_AGENT,
      timeoutMs: 30 * MIN,
      depth: 1,
    });
    const advanceTo = (ms: number) => {
      while (clock.now() < ms) clock.advanceBy(MIN);
    };
    advanceTo(6 * HOUR);
    const by6h = at.length;
    advanceTo(24 * HOUR);
    const by24h = at.length;
    advanceTo(72 * HOUR);
    const by72h = at.length;
    expect(handle.status()).toBe("running");
    const gaps = at.slice(1).map((t, i) => t - at[i]);
    expect({
      by6h,
      firstGapsMin: gaps.slice(0, 2).map((gap) => gap / MIN),
      backsOff: gaps.length >= 2 && gaps[1] > gaps[0],
      stops: by72h === by24h,
    }).toEqual({
      by6h,
      firstGapsMin: gaps.slice(0, 2).map((gap) => gap / MIN),
      backsOff: true,
      stops: true,
    });
    handle.dispose();
  });
});

describe("a silent monitor that times out", () => {
  it.fails("is reported without starting a turn", async () => {
    const clock = new ManualClock();
    const sent: Array<{ details?: unknown; options: { triggerTurn?: boolean; deliverAs?: string } }> = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_quiet", pid: 1 }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = new NotifyCenter({
      sendMessage: (message, options) => sent.push({ details: message.details, options }),
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
    try {
      const { timeoutMs } = await registry.start(
        { command: "tail -f build.log | grep ERROR", description: "build errors" },
        { cwd: home } as ExtensionContext,
      );
      expect(timeoutMs).toBe(5 * MIN);
      clock.advanceBy(timeoutMs!);
      await settleAsync();
      const timeout = sent.find((entry) => (entry.details as { status?: string } | undefined)?.status === "timeout");
      expect({ delivered: Boolean(timeout), startsTurn: timeout?.options.triggerTurn === true }).toEqual({
        delivered: true,
        startsTurn: false,
      });
    } finally {
      center.dispose();
      registry.disposeAll();
    }
  });
});
