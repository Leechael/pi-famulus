/**
 * Child lifecycle contract (design.md §4.6 "Child lifecycle"), seen the way
 * the parent sees it: through the registry's run records, plus the child's
 * running tool signal (aborting it is what stops a child's foreground shell).
 *
 * These paths must hold across the soft-deadline change: a child that
 * finishes, is interrupted, stalls and recovers, is resumed or is disposed
 * behaves the same whether timeout_ms aborts or only warns.
 */
import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { SubagentRegistry } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import type { ChildRunRequest, DisposableChildHandle } from "../../src/subagent/types";
import { SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

const MIN = 60_000;

function makeStack(creationGate?: Promise<void>, exposeHandle = true) {
  const clock = new ManualClock();
  const registry = new SubagentRegistry({ clock });
  const factory = new SessionFactory();
  factory.autoComplete = null;
  const runner = new InProcessRunner({
    createSession: async (req) => {
      if (creationGate) await creationGate;
      return factory.fn(req);
    },
    clock,
    stallMs: 5 * MIN,
    stallRetries: 1,
    stallRetryDelayMs: 5_000,
    acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
  });
  registry.setRunner(exposeHandle ? runner : { start: (req) => runner.start(req) });
  const transitions: string[] = [];
  registry.onTransition((run) => {
    const line = run.children.map((c) => c.status).join(",");
    if (transitions[transitions.length - 1] !== line) transitions.push(line);
  });
  const run = registry.createRun("tasks");
  const childId = registry.addChild(run.runId, { name: "w", agent: "worker" });
  const req: ChildRunRequest = {
    childId,
    runId: run.runId,
    name: "w",
    prompt: "do w",
    agent: WORKER_AGENT,
    timeoutMs: 30 * MIN,
    depth: 1,
  };
  const status = () => registry.get(run.runId)!.children[0].status;
  return { clock, registry, factory, req, status, transitions, runId: run.runId };
}

describe("child lifecycle", () => {
  it("finishes inside its budget: completed once, nothing fires afterwards", async () => {
    const { clock, registry, factory, req, status, transitions } = makeStack();
    const handle = await registry.startChild(req);
    expect(status()).toBe("running");
    for (let i = 0; i < 7; i++) {
      clock.advanceBy(4 * MIN); // active child: events inside every stall window
      factory.sessions[0].event();
    } // t = 28 min, inside the 30 min budget
    factory.sessions[0].complete("ok");
    const result = await handle.result;
    await tick();
    expect(result.status).toBe("completed");
    expect(clock.pendingTimers).toBe(0); // settle cleared every child timer
    clock.advanceBy(10 * 60 * MIN); // well past budget, stall, and any repeat
    await tick();
    expect(status()).toBe("completed");
    expect(factory.sessions[0].aborts).toBe(0);
    expect(transitions).toEqual(["", "pending", "running", "completed"]);
  });

  it("a long tool call inside the budget is never aborted (stall paused during tools)", async () => {
    const { clock, registry, factory, req, status } = makeStack();
    const handle = await registry.startChild(req);
    const tool = factory.sessions[0].runTool();
    clock.advanceBy(25 * MIN); // 5x stallMs, still inside the 30 min budget
    expect(tool.signal.aborted).toBe(false);
    expect(status()).toBe("running");
    tool.end();
    factory.sessions[0].complete("ok");
    expect((await handle.result).status).toBe("completed");
  });

  it("interrupt aborts the running tool and settles interrupted; later timers change nothing", async () => {
    const { clock, registry, factory, req, status } = makeStack();
    const handle = await registry.startChild(req);
    const tool = factory.sessions[0].runTool();
    clock.advanceBy(MIN);
    await handle.interrupt();
    const result = await handle.result;
    await tick();
    expect(result.status).toBe("interrupted");
    expect(result.error).toBeUndefined();
    expect(tool.signal.aborted).toBe(true);
    expect(clock.pendingTimers).toBe(0);
    clock.advanceBy(10 * 60 * MIN);
    await tick();
    expect(status()).toBe("interrupted");
    expect(factory.sessions[0].aborts).toBe(1);
  });

  it("a stall inside the budget auto-resumes on the same session and completes", async () => {
    const { clock, registry, factory, req, status } = makeStack();
    const handle = await registry.startChild(req);
    clock.advanceBy(5 * MIN); // stall detected, abort, retry after 5s
    await tick();
    clock.advanceBy(5_000);
    await tick();
    expect(status()).toBe("running");
    expect(factory.sessions).toHaveLength(1);
    expect(factory.sessions[0].prompts).toHaveLength(2);
    factory.sessions[0].complete("recovered");
    const result = await handle.result;
    expect(result).toMatchObject({ status: "completed", text: "recovered", stalls: 1 });
  });

  it("resume starts a new turn on the same session and settles again", async () => {
    const { clock, registry, factory, req, status, runId } = makeStack();
    const handle = await registry.startChild(req);
    factory.sessions[0].complete("first");
    await handle.result;
    await tick();
    expect(status()).toBe("completed");
    await handle.resume("again");
    await tick();
    expect(status()).toBe("running");
    clock.advanceBy(MIN);
    factory.sessions[0].complete("second");
    const second = await registry.getResult(req.childId)!;
    await tick();
    expect(second).toMatchObject({ status: "completed", text: "second", attempts: 2 });
    expect(registry.get(runId)!.children[0].status).toBe("completed");
  });

  it("disposal during session creation awaits late cleanup exactly once without prompting", async () => {
    let finishCreation!: () => void;
    const gate = new Promise<void>((resolve) => { finishCreation = resolve; });
    const { registry, factory, req } = makeStack(gate);
    let finishCleanup!: () => void;
    const cleanup = new Promise<void>((resolve) => { finishCleanup = resolve; });
    let disposalCalls = 0;
    factory.configure = (session) => {
      session.dispose = () => { disposalCalls++; return cleanup; };
    };
    const starting = registry.startChild(req);
    await tick();
    expect(factory.sessions).toHaveLength(0);
    let disposed = false;
    const disposal = registry.disposeAll().then(() => { disposed = true; });
    expect(registry.list()).toEqual([]);
    await tick();
    expect(disposed).toBe(false);
    finishCreation();
    await tick();
    expect(factory.sessions).toHaveLength(1);
    expect(disposalCalls).toBe(1);
    expect(factory.sessions[0].prompts).toEqual([]);
    expect(disposed).toBe(false);
    finishCleanup();
    await disposal;
    const handle = await starting;
    expect((await handle.result).status).toBe("interrupted");
    expect(disposalCalls).toBe(1);
  });

  it("awaits late handles from custom runners that ignore the startup callback", async () => {
    let finishCreation!: () => void;
    const gate = new Promise<void>((resolve) => { finishCreation = resolve; });
    const { registry, factory, req } = makeStack(gate, false);
    let finishCleanup!: () => void;
    const cleanup = new Promise<void>((resolve) => { finishCleanup = resolve; });
    let disposalCalls = 0;
    factory.configure = (session) => {
      session.dispose = () => { disposalCalls++; return cleanup; };
    };
    const starting = registry.startChild(req);
    await tick();
    let disposed = false;
    const disposal = registry.disposeAll().then(() => { disposed = true; });
    expect(registry.list()).toEqual([]);
    await tick();
    expect(disposed).toBe(false);
    finishCreation();
    await tick();
    expect(disposalCalls).toBe(1);
    expect(disposed).toBe(false);
    finishCleanup();
    await disposal;
    expect((await (await starting).result).status).toBe("interrupted");
    expect(disposalCalls).toBe(1);
  });

  it("runner disposal settles immediately but awaits asynchronous session cleanup", async () => {
    const { clock, registry, factory, req } = makeStack();
    let finishCleanup!: () => void;
    const cleanup = new Promise<void>((resolve) => { finishCleanup = resolve; });
    factory.configure = (session) => { session.dispose = () => cleanup; };
    const handle = await registry.startChild(req) as DisposableChildHandle;
    let disposed = false;
    const disposal = Promise.resolve(handle.dispose()).then(() => { disposed = true; });
    expect((await handle.result).status).toBe("interrupted");
    expect(clock.pendingTimers).toBe(0);
    await tick();
    expect(disposed).toBe(false);
    finishCleanup();
    await disposal;
    expect(disposed).toBe(true);
  });

  it.each(["disposeRun", "disposeAll"] as const)(
    "%s removes runs immediately but waits for every child cleanup",
    async (method) => {
      const { clock, registry, factory, req, runId } = makeStack();
      const finishCleanup: Array<() => void> = [];
      factory.configure = (session) => {
        const cleanup = new Promise<void>((resolve) => { finishCleanup.push(resolve); });
        session.dispose = () => cleanup;
      };
      const first = await registry.startChild(req);
      const otherRun = method === "disposeAll" ? registry.createRun("tasks") : registry.get(runId)!;
      const childId = registry.addChild(otherRun.runId, { name: "second", agent: "worker" });
      const second = await registry.startChild({ ...req, childId, runId: otherRun.runId });
      let disposed = false;
      const disposal = Promise.resolve(
        method === "disposeAll" ? registry.disposeAll() : registry.disposeRun(runId),
      ).then(() => { disposed = true; });
      expect(registry.list()).toEqual([]);
      expect(clock.pendingTimers).toBe(0);
      expect((await first.result).status).toBe("interrupted");
      expect((await second.result).status).toBe("interrupted");
      await tick();
      expect(disposed).toBe(false);
      finishCleanup[0]();
      await tick();
      expect(disposed).toBe(false);
      finishCleanup[1]();
      await disposal;
      expect(disposed).toBe(true);
    },
  );

  it.each(["sync", "async"])("runner propagates %s cleanup failures after settling", async (mode) => {
    const { clock, registry, factory, req } = makeStack();
    const failure = new Error("cleanup failed");
    factory.configure = (session) => {
      session.dispose = () => {
        if (mode === "sync") throw failure;
        return Promise.reject(failure);
      };
    };
    const handle = await registry.startChild(req) as DisposableChildHandle;
    if (mode === "sync") expect(() => handle.dispose()).toThrow(failure);
    else await expect(handle.dispose()).rejects.toBe(failure);
    expect((await handle.result).status).toBe("interrupted");
    expect(clock.pendingTimers).toBe(0);
  });

  it.each(["disposeRun", "disposeAll"] as const)(
    "%s propagates cleanup rejection only after all sessions finish",
    async (method) => {
      const { registry, factory, req, runId } = makeStack();
      const failure = new Error("extension shutdown failed");
      let finishCleanup!: () => void;
      factory.configure = (session, request) => {
        session.dispose = request.childId === req.childId
          ? () => Promise.reject(failure)
          : () => new Promise<void>((resolve) => { finishCleanup = resolve; });
      };
      await registry.startChild(req);
      const otherRun = method === "disposeAll" ? registry.createRun("tasks") : registry.get(runId)!;
      const childId = registry.addChild(otherRun.runId, { name: "second", agent: "worker" });
      await registry.startChild({ ...req, childId, runId: otherRun.runId });
      let settled = false;
      const outcome = (method === "disposeAll" ? registry.disposeAll() : registry.disposeRun(runId))
        .then(() => { throw new Error("expected cleanup failure"); }, (error) => {
          settled = true;
          return error;
        });
      await tick();
      expect(settled).toBe(false);
      expect(registry.list()).toEqual([]);
      finishCleanup();
      const error = await outcome;
      expect(error).toBeInstanceOf(AggregateError);
      if (method === "disposeRun") expect(error.errors).toEqual([failure]);
      else expect(error.errors[0].errors).toEqual([failure]);
    },
  );

  it("disposeRun settles the child interrupted and clears the generation timers", async () => {
    const { clock, registry, factory, req, status, runId } = makeStack();
    const handle = await registry.startChild(req);
    expect(status()).toBe("running");
    expect(clock.pendingTimers).toBeGreaterThan(0); // budget + stall armed
    registry.disposeRun(runId);
    const result = await handle.result;
    expect(result.status).toBe("interrupted");
    expect(factory.sessions[0].disposed).toBe(true);
    expect(clock.pendingTimers).toBe(0);
  });
});
