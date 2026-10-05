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
    acquire: (req) => registry.admitChild(req.childId),
  });
  registry.setRunner(runner);
  return { registry, factory, runner };
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
    factory.sessions[0].complete();
    await h1.result;
    await tick();
    expect(registry.activeChildren().map((c) => c.name)).toEqual(["b"]);
  });
});
