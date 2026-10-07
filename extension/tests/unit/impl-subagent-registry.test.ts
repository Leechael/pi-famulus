import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { SubagentRegistry, type RunRecord } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import type { ChildRunRequest } from "../../src/subagent/types";
import { SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

function makeStack(opts: { maxConcurrentChildren?: number; spawnBudgetPerHour?: number } = {}) {
  const clock = new ManualClock();
  const registry = new SubagentRegistry({
    maxConcurrentChildren: opts.maxConcurrentChildren ?? 8,
    spawnBudgetPerHour: opts.spawnBudgetPerHour ?? 32,
    clock,
  });
  const factory = new SessionFactory();
  const runner = new InProcessRunner({
    createSession: factory.fn,
    clock,
    acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
  });
  registry.setRunner(runner);
  return { registry, factory, runner, clock };
}

function addReq(registry: SubagentRegistry, runId: string, name: string): ChildRunRequest {
  const childId = registry.addChild(runId, { name, agent: "worker" });
  return {
    childId,
    runId,
    name,
    prompt: `do ${name}`,
    agent: WORKER_AGENT,
    timeoutMs: 60_000,
    depth: 1,
  };
}

describe("SubagentRegistry", () => {
  it("generates run/child ids in the contractual format", async () => {
    const { registry } = makeStack();
    const run = registry.createRun("tasks");
    expect(run.runId).toMatch(/^run_[0-9a-f]{8}$/);
    const req = addReq(registry, run.runId, "a");
    expect(req.childId).toMatch(/^ch_[0-9a-f]{8}$/);
    const run2 = registry.createRun("chain");
    expect(run2.runId).not.toBe(run.runId);
  });

  it("records the effective thinking level when an agent model falls back", async () => {
    const { registry, factory } = makeStack();
    factory.configure = (session) => {
      session.resolvedModel = "openai/gpt-5.6-sol";
      session.effectiveThinkingLevel = "low";
    };
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "fallback");
    req.agent = { ...WORKER_AGENT, model: "openai:high" };
    await registry.startChild(req);
    expect(registry.get(run.runId)?.children[0].model).toBe("openai/gpt-5.6-sol:low");
  });

  it("tracks pending -> running -> completed transitions with onTransition", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null; // keep the child running until we complete it
    const seen: { status: string; children: string }[] = [];
    registry.onTransition((run: RunRecord) => {
      seen.push({ status: run.status, children: run.children.map((c) => c.status).join(",") });
    });
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    req.agent = { ...WORKER_AGENT, systemPrompt: "agent preamble" };
    req.prompt = "agent preamble\n\n---\n\ndo a";
    req.taskPrompt = "do a";
    const handle = await registry.startChild(req);
    expect(handle.status()).toBe("running");
    expect(factory.sessions[0].prompts).toEqual(["agent preamble\n\n---\n\ndo a"]);
    factory.sessions[0].complete("done");
    await handle.result;
    await tick();

    const record = registry.get(run.runId)!;
    expect(record.status).toBe("completed");
    expect(record.children[0].status).toBe("completed");
    expect(record.children[0].result?.text).toBe("done");
    expect(record.children[0].prompt).toBe("do a");
    expect(record.children[0].preamble).toBe("agent preamble");
    expect(record.children[0].endedAt).toBeTypeOf("number");
    // Transitions observed: run creation, addChild, running, completed.
    expect(seen.some((s) => s.children === "pending")).toBe(true);
    expect(seen.some((s) => s.children === "running")).toBe(true);
    expect(seen.some((s) => s.status === "completed")).toBe(true);
  });

  it("run status is partial when children are mixed", async () => {
    const { registry, factory } = makeStack();
    factory.configure = (session, req) => {
      if (req.name === "bad") session.promptError = new Error("boom");
    };
    const run = registry.createRun("tasks");
    const h1 = await registry.startChild(addReq(registry, run.runId, "good"));
    const h2 = await registry.startChild(addReq(registry, run.runId, "bad"));
    await Promise.all([h1.result, h2.result]);
    await tick();
    const record = registry.get(run.runId)!;
    expect(record.status).toBe("partial");
    expect(record.children.map((c) => c.status)).toEqual(["completed", "failed"]);
  });

  it("enforces the global concurrency cap with queueing (no rejection)", async () => {
    const { registry, factory } = makeStack({ maxConcurrentChildren: 2 });
    factory.autoComplete = null; // manual completion
    const run = registry.createRun("tasks");
    const h1 = await registry.startChild(addReq(registry, run.runId, "a"));
    const h2 = await registry.startChild(addReq(registry, run.runId, "b"));
    // Third child queues for a slot: startChild does not resolve yet.
    let thirdStarted = false;
    const p3 = registry.startChild(addReq(registry, run.runId, "c")).then((h) => {
      thirdStarted = true;
      return h;
    });
    await tick();
    expect(thirdStarted).toBe(false);
    expect(factory.sessions).toHaveLength(2);
    expect(registry.activeChildren().map((c) => c.name)).toEqual(["a", "b", "c"]);

    factory.sessions[0].complete("done-a");
    await h1.result;
    const h3 = await p3;
    expect(thirdStarted).toBe(true);
    expect(factory.sessions).toHaveLength(3);
    expect(h2.status()).toBe("running");
    expect(h3.status()).toBe("running");
    factory.sessions[1].complete();
    factory.sessions[2].complete();
    await Promise.all([h2.result, h3.result]);
  });

  it("reports initial admission wait before the child handle is attached", async () => {
    const { registry, factory, clock } = makeStack({ maxConcurrentChildren: 1 });
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const first = await registry.startChild(addReq(registry, run.runId, "first"));
    const queued = registry.startChild(addReq(registry, run.runId, "queued"));
    await tick();

    clock.advanceBy(777);
    const snapshot = registry.get(run.runId)!.children.find((child) => child.name === "queued")!;
    expect(snapshot.status).toBe("pending");
    expect(snapshot.wallUsage?.queueMs).toBe(777);

    factory.sessions[0].complete("first done");
    await first.result;
    const second = await queued;
    expect(registry.get(run.runId)!.children.find((child) => child.name === "queued")!.wallUsage?.queueMs).toBe(777);
    factory.sessions[1].complete("second done");
    await second.result;
  });

  it("reports post-admission session startup as approximate other wall time before handle attachment", async () => {
    const { registry, factory, clock } = makeStack();
    let allowCreate!: () => void;
    let createStarted!: () => void;
    const createGate = new Promise<void>((resolve) => { allowCreate = resolve; });
    const startup = new Promise<void>((resolve) => { createStarted = resolve; });
    registry.setRunner(new InProcessRunner({
      createSession: async (req) => {
        createStarted();
        await createGate;
        return factory.fn(req);
      },
      clock,
      acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
    }));

    const run = registry.createRun("tasks");
    const starting = registry.startChild(addReq(registry, run.runId, "starting"));
    await startup;
    clock.advanceBy(777);
    const snapshot = registry.get(run.runId)!.children[0]!;
    expect(snapshot.status).toBe("running");
    expect(snapshot.wallUsage).toMatchObject({ queueMs: 0, otherMs: 777, approximate: true });

    allowCreate();
    const handle = await starting;
    await handle.result;
  });

  it("enforces the session spawn budget per hour", async () => {
    const { registry, factory } = makeStack({ spawnBudgetPerHour: 2 });
    const run = registry.createRun("tasks");
    const h1 = await registry.startChild(addReq(registry, run.runId, "a"));
    const h2 = await registry.startChild(addReq(registry, run.runId, "b"));
    const h3 = await registry.startChild(addReq(registry, run.runId, "c"));
    const r3 = await h3.result;
    expect(r3.status).toBe("failed");
    expect(r3.error).toContain("spawn budget exhausted");
    expect(factory.sessions).toHaveLength(2); // no third session was created
    await Promise.all([h1.result, h2.result]);
    await tick();
    const record = registry.get(run.runId)!;
    expect(record.children[2].status).toBe("failed");
    expect(record.status).toBe("partial");
  });

  it("findChild resolves by id and by name; lineage is runId equality", async () => {
    const { registry } = makeStack();
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "alpha");
    const handle = await registry.startChild(req);
    expect(registry.findChild(run.runId, req.childId)?.childId).toBe(req.childId);
    expect(registry.findChild(run.runId, "alpha")?.childId).toBe(req.childId);
    expect(registry.findChild(run.runId, "nope")).toBeUndefined();
    expect(registry.lineage(run.runId, run.runId)).toBe(true);
    const other = registry.createRun("tasks");
    expect(registry.lineage(run.runId, other.runId)).toBe(false);
    await handle.result;
  });

  it("handle() and getResult() reach finished children until dispose", async () => {
    const { registry } = makeStack();
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    const handle = await registry.startChild(req);
    await handle.result;
    expect(registry.handle(req.childId)).toBe(handle);
    await expect(registry.getResult(req.childId)).resolves.toMatchObject({ status: "completed" });
  });

  it("finalizeRun marks never-submitted pending children interrupted", async () => {
    const { registry } = makeStack();
    const run = registry.createRun("tasks");
    addReq(registry, run.runId, "a");
    addReq(registry, run.runId, "b");
    registry.finalizeRun(run.runId, "cancelled (fail_fast)");
    const record = registry.get(run.runId)!;
    expect(record.children.every((c) => c.status === "interrupted")).toBe(true);
    expect(record.children[0].result?.error).toBe("cancelled (fail_fast)");
    expect(record.status).toBe("interrupted");
  });

  it("activeChildren includes a pending child's current queue delta", () => {
    const { registry, clock } = makeStack();
    const run = registry.createRun("tasks");
    const childId = registry.addChild(run.runId, { name: "queued", agent: "worker" });
    clock.advanceBy(75);

    expect(registry.activeChildren().find((child) => child.childId === childId)).toMatchObject({
      status: "pending",
      queueMs: 75,
    });
  });

  it("finalizeRun records elapsed queue time for never-submitted pending children", () => {
    const { registry, clock } = makeStack();
    const run = registry.createRun("tasks");
    registry.addChild(run.runId, { name: "queued", agent: "worker" });
    clock.advanceBy(75);
    expect(registry.activeChildren()[0]).toMatchObject({ status: "pending", queueMs: 75 });
    let terminal: RunRecord | undefined;
    registry.onTransition((record) => { terminal = record; });

    registry.finalizeRun(run.runId, "cancelled (fail_fast)");

    expect(terminal?.children[0]).toMatchObject({ status: "interrupted", queueMs: 75 });
  });

  it("finalizeRun freezes queue wall usage for never-submitted pending children", () => {
    const { registry, clock } = makeStack();
    const run = registry.createRun("tasks");
    registry.addChild(run.runId, { name: "queued", agent: "worker" });
    clock.advanceBy(75);

    registry.finalizeRun(run.runId, "cancelled (fail_fast)");

    const queueAtFinalize = registry.get(run.runId)!.children[0]!.wallUsage!.queueMs;
    clock.advanceBy(500);
    const frozen = registry.get(run.runId)!.children[0]!;
    expect(queueAtFinalize).toBe(75);
    expect(frozen.queueMs).toBe(75);
    expect(frozen.wallUsage).toMatchObject({ queueMs: queueAtFinalize, otherMs: 0 });
  });

  it("a queued child cancelled via shouldStart settles as interrupted", async () => {
    const { registry, factory } = makeStack({ maxConcurrentChildren: 1 });
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const h1 = await registry.startChild(addReq(registry, run.runId, "a"));
    let cancelled = false;
    const p2 = registry.startChild(addReq(registry, run.runId, "b"), {
      shouldStart: () => !cancelled,
    });
    await tick();
    expect(factory.sessions).toHaveLength(1);
    cancelled = true;
    factory.sessions[0].complete("done-a"); // frees the slot -> admission re-checks
    await h1.result;
    const h2 = await p2;
    const r2 = await h2.result;
    expect(r2.status).toBe("interrupted");
    expect(r2.error).toContain("fail_fast");
    expect(factory.sessions).toHaveLength(1); // never spawned
  });

  it("resume re-runs the child and the registry tracks the new generation", async () => {
    const { registry, factory } = makeStack();
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    const handle = await registry.startChild(req);
    await handle.result;
    expect(registry.get(run.runId)!.status).toBe("completed");

    factory.sessions[0].autoComplete = null;
    await registry.resumeChild(req.childId, "continue");
    await tick();
    expect(registry.get(run.runId)!.status).toBe("running");
    expect(registry.get(run.runId)!.children[0].status).toBe("running");

    const second = registry.getResult(req.childId)!;
    factory.sessions[0].complete("second");
    await expect(second).resolves.toMatchObject({ status: "completed", text: "second" });
    await tick();
    const record = registry.get(run.runId)!;
    expect(record.status).toBe("completed");
    expect(record.children[0].result?.text).toBe("second");
    expect(record.children[0].turn).toBe(2);
  });

  it("a concurrent rejected resume does not inflate turn", async () => {
    // handle.resume is async: a second overlapping resumeChild rejects
    // (still pending) as a promise, after it has already incremented turn.
    // Admission of the first can set the child running before that rejection
    // is caught; rolling back only when status is still pending would leave
    // turn at 3 for one accepted resume.
    const { registry, factory } = makeStack();
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    const handle = await registry.startChild(req);
    await handle.result;
    expect(registry.get(run.runId)!.children[0].turn).toBe(1);

    factory.sessions[0].autoComplete = null;
    const first = registry.resumeChild(req.childId, "one");
    const second = registry.resumeChild(req.childId, "two");
    await expect(second).rejects.toThrow(/still pending|use steer/);
    await first;
    await tick();
    expect(registry.get(run.runId)!.children[0].status).toBe("running");
    expect(registry.get(run.runId)!.children[0].turn).toBe(2);
  });

  it("a rejected resume of a running child restores turn and status", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    await registry.startChild(req);
    expect(registry.get(run.runId)!.children[0]).toMatchObject({ status: "running", turn: 1 });
    await expect(registry.resumeChild(req.childId, "nope")).rejects.toThrow(/still running|use steer/);
    expect(registry.get(run.runId)!.children[0]).toMatchObject({ status: "running", turn: 1 });
  });

  it("records elapsed admission wait when disposing a queued child", () => {
    const { registry, clock } = makeStack();
    const run = registry.createRun("tasks");
    registry.addChild(run.runId, { name: "queued", agent: "worker" });
    clock.advanceBy(75);
    let terminal: RunRecord | undefined;
    registry.onTransition((record) => { terminal = record; });

    registry.disposeRun(run.runId);

    expect(terminal?.children[0]).toMatchObject({ status: "interrupted", queueMs: 75 });
  });

  it("disposeRun interrupts children, disposes sessions, and removes the run", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    const handle = await registry.startChild(req);
    registry.disposeRun(run.runId);
    expect(registry.get(run.runId)).toBeUndefined();
    expect(registry.handle(req.childId)).toBeUndefined();
    expect(registry.activeChildren()).toEqual([]);
    expect(factory.sessions[0].disposed).toBe(true);
    const result = await handle.result;
    expect(result.status).toBe("interrupted");
  });

  it("emits interrupted before disposeRun drops the children", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null;
    const seen: string[] = [];
    registry.onTransition((run) => {
      seen.push(run.children.map((c) => c.status).join(","));
    });
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "a");
    await registry.startChild(req);
    registry.disposeRun(run.runId);
    expect(seen.at(-1)).toBe("interrupted");
  });

  it("disposeAll disposes every run", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null;
    const run1 = registry.createRun("tasks");
    const run2 = registry.createRun("chain");
    await registry.startChild(addReq(registry, run1.runId, "a"));
    await registry.startChild(addReq(registry, run2.runId, "b"));
    registry.disposeAll();
    expect(registry.list()).toEqual([]);
    expect(factory.sessions.every((s) => s.disposed)).toBe(true);
  });

  it("activeChildren lists pending and running children only", async () => {
    const { registry, factory } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const h1 = await registry.startChild(addReq(registry, run.runId, "a"));
    await registry.startChild(addReq(registry, run.runId, "b"));
    expect(registry.activeChildren().map((c) => c.name)).toEqual(["a", "b"]);
    expect(registry.activeChildren()[0]).toMatchObject({
      childId: h1.childId,
      workKind: "other",
      queueMs: 0,
    });
    factory.sessions[0].complete();
    await h1.result;
    await tick();
    expect(registry.activeChildren().map((c) => c.name)).toEqual(["b"]);
  });
});
