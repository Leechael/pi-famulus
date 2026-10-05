import { describe, expect, it, vi } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import type { ManagerClient } from "../../src/manager-client";
import { createChildBashTool } from "../../src/subagent/child-bash";
import { buildOverrunInfo, ChildShellTracker, type OverrunInfoDeps } from "../../src/subagent/overrun";

const MIN = 60_000;

function deps(clock: ManualClock, overrides: Partial<OverrunInfoDeps> = {}): OverrunInfoDeps {
  return {
    now: () => clock.now(),
    lookupChild: (id) => (id === "ch_a" ? { runId: "run_a", name: "worker-1" } : undefined),
    conversation: () => [
      { role: "user", text: "do it" },
      { role: "assistant", text: "Running the suite." },
    ],
    shells: new ChildShellTracker(),
    stat: () => null,
    ...overrides,
  };
}

const tick = { childId: "ch_a", elapsedMs: 30 * MIN, budgetMs: 30 * MIN, reminder: 1, nextReminderMs: 10 * MIN };

describe("buildOverrunInfo", () => {
  it("names the child's run, turn time, and last transcript activity", () => {
    const clock = new ManualClock(100 * MIN);
    const info = buildOverrunInfo(deps(clock), { ...tick, lastEventAt: 97 * MIN });
    expect(info).toEqual({
      runId: "run_a",
      childId: "ch_a",
      name: "worker-1",
      elapsedMs: 30 * MIN,
      budgetMs: 30 * MIN,
      reminder: 1,
      nextReminderMs: 10 * MIN,
      lastActivity: { agoMs: 3 * MIN, text: "assistant: Running the suite." },
    });
  });

  it("returns undefined once the child's run is gone (no wake for a disposed run)", () => {
    const clock = new ManualClock();
    expect(buildOverrunInfo(deps(clock), { ...tick, childId: "ch_gone", lastEventAt: 0 })).toBeUndefined();
  });

  it("describes the blocking shell; growth comes from size between readings, else a recent mtime", () => {
    const clock = new ManualClock(0);
    let file = { size: 100, mtimeMs: 0 };
    const d = deps(clock, { stat: () => file });
    d.shells.start("ch_a", { taskId: "sh_1", command: "npm test", startedAt: 0, outputPath: "/o/sh_1.output" });

    clock.advanceBy(12 * MIN); // first reading: file last changed 12 min ago
    let shell = buildOverrunInfo(d, { ...tick, lastEventAt: 0 })!.shell!;
    expect(shell).toEqual({
      taskId: "sh_1",
      command: "npm test",
      elapsedMs: 12 * MIN,
      outputPath: "/o/sh_1.output",
      outputBytes: 100,
      outputIdleMs: 12 * MIN,
      growing: false,
    });

    file = { size: 250, mtimeMs: 13 * MIN };
    clock.advanceBy(10 * MIN); // grew since the last reading, though not in the last minute
    shell = buildOverrunInfo(d, { ...tick, lastEventAt: 0 })!.shell!;
    expect(shell.growing).toBe(true);
    expect(shell.outputIdleMs).toBe(9 * MIN);

    clock.advanceBy(10 * MIN); // unchanged since the previous reading
    expect(buildOverrunInfo(d, { ...tick, lastEventAt: 0 })!.shell!.growing).toBe(false);

    file = { size: 250, mtimeMs: clock.now() - 5_000 }; // touched seconds ago
    expect(buildOverrunInfo(d, { ...tick, lastEventAt: 0 })!.shell!.growing).toBe(true);
  });

  it("reports unknown output facts as null when the file cannot be read", () => {
    const clock = new ManualClock(0);
    const d = deps(clock);
    d.shells.start("ch_a", { taskId: "sh_1", command: "make", startedAt: 0, outputPath: "/missing" });
    const shell = buildOverrunInfo(d, { ...tick, lastEventAt: 0 })!.shell!;
    expect(shell).toMatchObject({ outputBytes: null, outputIdleMs: null, growing: null });
  });

  it("drops a shell once it ended, and ignores an end for a different task", () => {
    const t = new ChildShellTracker();
    t.start("ch_a", { taskId: "sh_1", command: "a", startedAt: 0, outputPath: "" });
    t.end("ch_a", "sh_old");
    expect(t.current("ch_a")?.taskId).toBe("sh_1");
    t.end("ch_a", "sh_1");
    expect(t.current("ch_a")).toBeUndefined();
  });
});

describe("child bash records the shell it is blocked on", () => {
  const ctx = { cwd: "/tmp" } as ExtensionToolContext;

  function client(wait: () => Promise<unknown>): ManagerClient {
    return {
      ensureAvailable: vi.fn(async () => true),
      start: vi.fn(async () => ({ task_id: "sh_9", pid: 1 })),
      wait: vi.fn(wait),
      output: vi.fn(async () => ({ chunk: "ok\n", next_cursor: 3, status: "completed", exit_code: 0, total_size: 3 })),
      stop: vi.fn(async () => {}),
    } as unknown as ManagerClient;
  }

  function tool(c: ManagerClient, shells: ChildShellTracker) {
    return createChildBashTool({
      getClient: () => c,
      home: "/h",
      sessionId: () => "sid",
      sessionEnv: () => ({}),
      trackTask: () => {},
      childId: "ch_a",
      runId: "run_a",
      clock: new ManualClock(0),
      shells,
    });
  }

  it("while the command runs, and not after it returns", async () => {
    const shells = new ChildShellTracker();
    let release!: (v: unknown) => void;
    const c = client(() => new Promise((r) => (release = r)));
    const pending = tool(c, shells).execute("tc", { command: "npm test" }, undefined, undefined, ctx);
    await vi.waitFor(() => expect(shells.current("ch_a")).toBeDefined());
    expect(shells.current("ch_a")).toMatchObject({
      taskId: "sh_9",
      command: "npm test",
      outputPath: "/h/sessions/sid/tasks/sh_9.output",
    });
    release({ done: true, exit_code: 0 });
    await pending;
    expect(shells.current("ch_a")).toBeUndefined();
  });

  it("and clears it when the call throws (abort)", async () => {
    const shells = new ChildShellTracker();
    const c = client(() => new Promise(() => {}));
    const controller = new AbortController();
    const pending = tool(c, shells).execute("tc", { command: "long" }, controller.signal, undefined, ctx);
    await vi.waitFor(() => expect(shells.current("ch_a")).toBeDefined());
    controller.abort();
    await expect(pending).rejects.toThrow(/aborted/);
    expect(shells.current("ch_a")).toBeUndefined();
  });
});
