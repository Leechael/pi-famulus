import { describe, expect, it, vi } from "vitest";
import { ManualClock } from "../../src/clock";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { SubagentRegistry } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import { createSubagentTool } from "../../src/subagent/tool";
import { FAMULUS_WAKE_CUSTOM_TYPE } from "../../src/wake";
import type { OverrunTick } from "../../src/subagent/overrun";
import { SessionFactory, tick } from "./subagent-fakes";

async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 16; i++) await Promise.resolve();
}

function makeStack(
  opts: { budgetMs?: number; autoComplete?: string | null; hardTimeoutMs?: number; maxConcurrentChildren?: number } = {},
) {
  const clock = new ManualClock();
  const registry = new SubagentRegistry({ clock, maxConcurrentChildren: opts.maxConcurrentChildren });
  const factory = new SessionFactory();
  factory.autoComplete = opts.autoComplete === undefined ? "done" : opts.autoComplete;
  const overruns: OverrunTick[] = [];
  const runner = new InProcessRunner({
    createSession: factory.fn,
    clock,
    stallMs: 0,
    overrunRepeatMs: 60_000,
    hardTimeoutMs: opts.hardTimeoutMs,
    onOverrun: (t) => overruns.push(t),
    acquire: (req) => registry.admitChild(req.childId),
  });
  registry.setRunner(runner);
  const notify = vi.fn();
  const tool = createSubagentTool({
    getRegistry: () => registry,
    getNotifyCenter: () => ({ notify }),
    budgetMs: () => opts.budgetMs ?? 45_000,
    defaultTimeoutMs: 600_000,
    defaultConcurrency: 4,
    clock,
  });
  const ctx = { cwd: "/tmp" } as ExtensionToolContext;
  const exec = (params: Record<string, unknown>, signal?: AbortSignal) =>
    tool.execute("tc", params as never, signal, undefined, ctx);
  return { registry, factory, notify, exec, clock, overruns };
}

describe("subagent tool — validation", () => {
  it("requires one of tasks/chain/action", async () => {
    const { exec } = makeStack();
    await expect(exec({})).rejects.toThrow(/one of tasks, chain, or action/);
  });

  it("tasks and chain are mutually exclusive", async () => {
    const { exec } = makeStack();
    await expect(
      exec({ tasks: [{ prompt: "a" }], chain: [{ prompt: "b" }] }),
    ).rejects.toThrow(/mutually exclusive/);
  });

  it("action is mutually exclusive with tasks/chain", async () => {
    const { exec } = makeStack();
    await expect(exec({ action: "list", tasks: [{ prompt: "a" }] })).rejects.toThrow(
      /mutually exclusive/,
    );
  });

  it("rejects an unknown agent name and lists available ones", async () => {
    const { exec } = makeStack();
    await expect(exec({ tasks: [{ agent: "nope", prompt: "a" }] })).rejects.toThrow(
      /unknown agent "nope" \(available: worker\)/,
    );
  });

  it("rejects unknown chain label references before starting", async () => {
    const { exec, factory } = makeStack();
    await expect(
      exec({ chain: [{ prompt: "a" }, { prompt: "{outputs.missing}" }] }),
    ).rejects.toThrow(/unknown label reference/);
    expect(factory.sessions).toHaveLength(0);
    // Nothing was registered as a run either.
    // (validation happens before createRun)
  });
});

describe("subagent tool — tasks", () => {
  it("runs tasks in parallel and returns ordinal-preserved sections", async () => {
    const { exec, factory, notify } = makeStack();
    const result = await exec({ tasks: [{ prompt: "a" }, { prompt: "b", name: "second" }] });
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("2/2 subagents completed");
    expect(text).toContain("## worker-1 (completed)");
    expect(text).toContain("## second (completed)");
    expect(text.indexOf("worker-1")).toBeLessThan(text.indexOf("second"));
    expect(factory.sessions).toHaveLength(2);
    expect(factory.sessions[0].prompts).toEqual(["a"]);
    expect(factory.sessions[1].prompts).toEqual(["b"]);
    const details = result.details as { run_id: string; status: string };
    expect(details.run_id).toMatch(/^run_[0-9a-f]{8}$/);
    expect(details.status).toBe("completed");
    // Synchronous delivery: no notification.
    expect(notify).not.toHaveBeenCalled();
  });

  it("a failed child does not drag down the group", async () => {
    const { exec, factory } = makeStack();
    factory.configure = (session, req) => {
      if (req.name === "worker-2") session.promptError = new Error("boom");
    };
    const result = await exec({ tasks: [{ prompt: "a" }, { prompt: "b" }] });
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("## worker-1 (completed)");
    expect(text).toContain("## worker-2 (failed)");
    expect(text).toContain("Error: boom");
    expect((result.details as { status: string }).status).toBe("partial");
  });

  it("fail_fast cancels not-yet-started children", async () => {
    const { exec, factory } = makeStack({ autoComplete: null });
    factory.configure = (session, req) => {
      if (req.name === "worker-2") session.promptError = new Error("boom");
    };
    const pending = exec({
      tasks: [{ prompt: "slow" }, { prompt: "fails" }, { prompt: "never" }],
      concurrency: 2,
      fail_fast: true,
    });
    // worker-1 is still running (manual); wait for the workers to spin up,
    // then complete it to let the run finish.
    await flushMicrotasks();
    expect(factory.sessions.length).toBeGreaterThanOrEqual(2);
    factory.sessions[0].complete("finally");
    const settled = await pending;
    const text = settled.content[0].type === "text" ? settled.content[0].text : "";
    expect(text).toContain("## worker-1 (completed)");
    expect(text).toContain("## worker-2 (failed)");
    expect(text).toContain("## worker-3 (interrupted)");
    expect(text).toContain("cancelled (fail_fast)");
    expect(factory.sessions).toHaveLength(2); // third never spawned
  });

  it("reports provider stopReason errors as failed in the result and wake", async () => {
    const { exec, factory, notify } = makeStack({ autoComplete: null });
    factory.configure = (session) => {
      session.lastAssistantFailure = { stopReason: "error", errorMessage: "529 overloaded_error" };
    };
    const result = await exec({ tasks: [{ prompt: "slow" }], async: true });
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(1);
    factory.sessions[0].complete();
    await flushMicrotasks();

    expect(notify).toHaveBeenCalledTimes(1);
    expect(notify.mock.calls[0][0].content).toContain('status="failed"');
    expect(notify.mock.calls[0][0].content).toContain("<error>529 overloaded_error</error>");
    expect(result.details).toMatchObject({ status: "backgrounded" });
  });

  it("backgrounds the run when the foreground budget elapses, then notifies", async () => {
    const { exec, factory, notify, clock } = makeStack({ budgetMs: 50, autoComplete: null });
    const pending = exec({ tasks: [{ prompt: "slow" }] });
    for (let i = 0; i < 8; i++) await Promise.resolve();
    clock.advanceBy(50);
    const result = await pending;
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("run_");
    expect(text).toContain("background");
    expect(text).toContain("Do not poll");
    expect((result.details as { status: string }).status).toBe("backgrounded");
    const runId = (result.details as { run_id: string }).run_id;

    // Complete the child later -> completion notification fires.
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(1);
    factory.sessions[0].complete("late result");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(1);
    const message = notify.mock.calls[0][0];
    expect(message.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(message.content).toContain('kind="subagent-done"');
    expect(message.content).toContain(`run-id="${runId}"`);
    expect(message.content).toContain('status="completed"');
    expect(message.content).toContain("late result");
    expect(message.details).toMatchObject({ kind: "subagent-done", runId });
  });

  it("async: true returns immediately and still notifies on completion", async () => {
    const { exec, factory, notify } = makeStack({ autoComplete: null });
    const result = await exec({ tasks: [{ prompt: "x" }, { prompt: "y" }], async: true });
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("Do not poll");
    expect((result.details as { status: string }).status).toBe("backgrounded");
    expect(notify).not.toHaveBeenCalled();
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(2);
    factory.sessions[0].complete("r1");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(1);
    const handover = notify.mock.calls[0][0].content as string;
    expect(handover).toContain('kind="subagent-handover"');
    expect(handover).toContain("<prompt>");
    expect(handover).toContain("r1");
    expect(handover).toContain("still running");
    factory.sessions[1].complete("r2");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(2);
    expect(notify.mock.calls[1][0].content).toContain("2/2 subagents completed");
  });

  it("async: true carries a child's warning into the handover and done wakes", async () => {
    // A warning only on the child result is invisible to an async parent: the
    // wakes are all it sees unless it fetches the full record.
    const warning = 'unknown thinking level "highest" in model spec "haiku:highest"; using the model\'s default thinking';
    const { exec, factory, notify } = makeStack({ autoComplete: null });
    factory.configure = (session, req) => {
      if (req.prompt === "x") session.warning = warning;
    };
    await exec({ tasks: [{ prompt: "x" }, { prompt: "y" }], async: true });
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(2);

    factory.sessions[0].complete("r1");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(1);
    const handover = notify.mock.calls[0][0];
    expect(handover.content).toContain('kind="subagent-handover"');
    expect(handover.content).toContain(`<warning>${warning}</warning>`);
    expect(handover.details).toMatchObject({ kind: "subagent-handover", warning });

    factory.sessions[1].complete("r2");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(2);
    const done = notify.mock.calls[1][0];
    expect(done.content).toContain('kind="subagent-done"');
    expect(done.content).toContain(`<warning>${warning}</warning>`);
    expect(done.content.match(/<warning>/g)).toHaveLength(1);
    const children = (done.details as { children: { warning?: string }[] }).children;
    expect(children[0].warning).toBe(warning);
    expect(children[1]).not.toHaveProperty("warning");
  });

  it("an aborted sync wait backgrounds the run rather than killing it", async () => {
    const { exec, factory } = makeStack({ autoComplete: null });
    const controller = new AbortController();
    const pending = exec({ tasks: [{ prompt: "slow" }] }, controller.signal);
    controller.abort();
    const result = await pending;
    expect((result.details as { status: string }).status).toBe("backgrounded");
    // The child is still running (not interrupted).
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(1);
    expect(factory.sessions[0].isStreaming()).toBe(true);
    factory.sessions[0].complete("done");
  });
});

describe("subagent tool — chain", () => {
  it("runs steps sequentially with interpolation", async () => {
    const { exec, factory } = makeStack();
    factory.autoComplete = "step-output";
    const result = await exec({
      chain: [
        { prompt: "first", label: "one" },
        { prompt: "second uses {outputs.one} and {previous}" },
      ],
    });
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("2/2 subagents completed");
    expect(factory.sessions[1].prompts).toEqual(["second uses step-output and step-output"]);
    // Sequential: the second session was created after the first completed.
    expect(factory.sessions).toHaveLength(2);
  });
});

describe("subagent tool — management actions", () => {
  it("list reports runs and their status", async () => {
    const { exec } = makeStack();
    const empty = await exec({ action: "list" });
    expect(empty.content[0].type === "text" && empty.content[0].text).toContain("No subagent runs");
    const started = await exec({ tasks: [{ prompt: "a" }] });
    const runId = (started.details as { run_id: string }).run_id;
    const listed = await exec({ action: "list" });
    const text = listed.content[0].type === "text" ? listed.content[0].text : "";
    expect(text).toContain(runId);
    expect(text).toContain("completed");
  });

  it("get returns the full results of a run", async () => {
    const { exec } = makeStack();
    const started = await exec({ tasks: [{ prompt: "a", name: "solo" }] });
    const runId = (started.details as { run_id: string }).run_id;
    const got = await exec({ action: "get", run_id: runId });
    const text = got.content[0].type === "text" ? got.content[0].text : "";
    expect(text).toContain("## solo (completed)");
    expect(text).toContain("done");
  });

  it("get/status reject unknown run_id with the known list", async () => {
    const { exec } = makeStack();
    await expect(exec({ action: "get", run_id: "run_nope0000" })).rejects.toThrow(/unknown run_id/);
  });

  it("status shows per-child state and elapsed time", async () => {
    const { exec, factory } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a", name: "longrunner" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    expect(factory.sessions).toHaveLength(1);
    const status = await exec({ action: "status", run_id: runId });
    const text = status.content[0].type === "text" ? status.content[0].text : "";
    expect(text).toContain("longrunner");
    expect(text).toContain("running");
    expect(text).toContain("last event");
    factory.sessions[0].complete("done");
  });

  it("interrupt aborts a running child", async () => {
    const { exec, factory, registry } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    // Wait until the handle is registered (implies the session is assigned
    // inside the handle and the prompt was issued).
    await flushMicrotasks();
    const rec = registry.get(runId)!;
    expect(rec.children).toHaveLength(1);
    expect(registry.handle(rec.children[0].childId)).toBeDefined();
    const result = await exec({ action: "interrupt", run_id: runId });
    const text = result.content[0].type === "text" ? result.content[0].text : "";
    expect(text).toContain("Interrupted 1 subagent(s)");
    expect(factory.sessions[0].aborts).toBe(1);
    const record = registry.get(runId)!;
    expect(record.children[0].status).toBe("interrupted");
    expect(record.status).toBe("interrupted");
  });

  it("interrupt with child_id only hits that child", async () => {
    const { exec, factory, registry } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a" }, { prompt: "b" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    const rec = registry.get(runId)!;
    expect(rec.children).toHaveLength(2);
    expect(registry.handle(rec.children[0].childId)).toBeDefined();
    expect(registry.handle(rec.children[1].childId)).toBeDefined();
    await exec({ action: "interrupt", run_id: runId, child_id: "worker-1" });
    expect(factory.sessions[0].aborts).toBe(1);
    expect(factory.sessions[1].aborts).toBe(0);
    factory.sessions[1].complete("b done");
  });

  it("steer delivers a message to the single running child", async () => {
    const { exec, factory, registry } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    const rec = registry.get(runId)!;
    expect(rec.children).toHaveLength(1);
    expect(registry.handle(rec.children[0].childId)).toBeDefined();
    const result = await exec({ action: "steer", run_id: runId, message: "focus on tests" });
    expect(factory.sessions[0].steers).toEqual(["focus on tests"]);
    expect(result.content[0].type === "text" && result.content[0].text).toContain("Steered");
    factory.sessions[0].complete("done");
  });

  it("steer requires a message and a running target", async () => {
    const { exec } = makeStack();
    const started = await exec({ tasks: [{ prompt: "a" }] });
    const runId = (started.details as { run_id: string }).run_id;
    await expect(exec({ action: "steer", run_id: runId })).rejects.toThrow(/message is required/);
    await expect(exec({ action: "steer", run_id: runId, message: "x" })).rejects.toThrow(
      /cannot steer/,
    );
  });

  it("resume re-prompts a finished child and notifies on completion", async () => {
    const { exec, factory, notify } = makeStack();
    const started = await exec({ tasks: [{ prompt: "a" }] });
    const runId = (started.details as { run_id: string }).run_id;
    expect(notify).not.toHaveBeenCalled(); // delivered synchronously

    factory.sessions[0].autoComplete = null;
    const resumed = await exec({ action: "resume", run_id: runId, message: "now do more" });
    expect(resumed.content[0].type === "text" && resumed.content[0].text).toContain("Resumed");
    expect(resumed.content[0].type === "text" && resumed.content[0].text).not.toContain("queued");
    expect(factory.sessions[0].prompts).toEqual(["a", "now do more"]);

    factory.sessions[0].complete("second result");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(1);
    expect(notify.mock.calls[0][0].content).toContain("second result");
  });

  it("resume requires the child to be terminal", async () => {
    const { exec, factory, registry } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    const rec = registry.get(runId)!;
    expect(rec.children).toHaveLength(1);
    expect(registry.handle(rec.children[0].childId)).toBeDefined();
    await expect(
      exec({ action: "resume", run_id: runId, child_id: "worker-1", message: "x" }),
    ).rejects.toThrow(/still running/);
    factory.sessions[0].complete("done");
  });
});

describe("subagent tool — extend (soft deadline)", () => {
  async function runningChild(opts: { hardTimeoutMs?: number } = {}) {
    const stack = makeStack({ autoComplete: null, ...opts });
    const started = await stack.exec({ tasks: [{ prompt: "a", name: "tester" }], async: true, timeout_ms: 10_000 });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    return { ...stack, runId };
  }

  it("re-arms a running child's deadline at now + timeout_ms", async () => {
    const { exec, clock, overruns, runId, registry } = await runningChild();
    clock.advanceBy(10_000);
    expect(overruns).toHaveLength(1);
    const res = await exec({ action: "extend", run_id: runId, child_id: "tester", timeout_ms: 120_000 });
    const text = res.content[0].type === "text" ? res.content[0].text : "";
    expect(text).toContain("Extended subagent tester");
    expect(text).toContain('the next <pi-famulus-wake kind="subagent-overrun"> comes in 2m0s if it is still running');
    expect(text).not.toContain("hard ceiling");
    clock.advanceBy(119_999);
    expect(overruns).toHaveLength(1); // the 60s repeat was replaced by the new deadline
    clock.advanceBy(1);
    expect(overruns[1]).toMatchObject({ reminder: 2, budgetMs: 130_000 });
    expect(registry.get(runId)!.children[0].status).toBe("running");
  });

  it("without timeout_ms gives the child its spawn budget again", async () => {
    const { exec, clock, overruns, runId } = await runningChild();
    clock.advanceBy(10_000);
    await exec({ action: "extend", run_id: runId }); // the only running child
    clock.advanceBy(9_999);
    expect(overruns).toHaveLength(1);
    clock.advanceBy(1);
    expect(overruns).toHaveLength(2);
  });

  it("says when a configured hard ceiling still applies", async () => {
    const { exec, clock, runId } = await runningChild({ hardTimeoutMs: 600_000 });
    clock.advanceBy(10_000);
    const res = await exec({ action: "extend", run_id: runId, timeout_ms: 3_600_000 });
    const text = res.content[0].type === "text" ? res.content[0].text : "";
    expect(text).toContain("The configured hard ceiling (hardTimeoutMs) is not moved: it stops this subagent in 9m50s.");
  });

  it("errors for a finished child", async () => {
    const { exec, factory, runId } = await runningChild();
    factory.sessions[0].complete("done");
    await flushMicrotasks();
    await expect(exec({ action: "extend", run_id: runId, child_id: "tester", timeout_ms: 60_000 })).rejects.toThrow(
      /not running/,
    );
  });
});

describe("subagent tool — resume honours timeout_ms", () => {
  async function finishedChild() {
    const stack = makeStack({ autoComplete: null });
    const started = await stack.exec({ tasks: [{ prompt: "a", name: "tester" }], async: true, timeout_ms: 10_000 });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    stack.factory.sessions[0].complete("first");
    await flushMicrotasks();
    return { ...stack, runId };
  }

  it("the resumed turn gets the timeout_ms passed with resume", async () => {
    const { exec, clock, overruns, runId } = await finishedChild();
    await exec({ action: "resume", run_id: runId, child_id: "tester", message: "more", timeout_ms: 30_000 });
    clock.advanceBy(29_999);
    expect(overruns).toHaveLength(0);
    clock.advanceBy(1);
    expect(overruns[0]).toMatchObject({ reminder: 1, budgetMs: 30_000 });
  });

  it("without timeout_ms the resumed turn gets the spawn budget, not the default", async () => {
    const { exec, clock, overruns, runId } = await finishedChild();
    await exec({ action: "resume", run_id: runId, child_id: "tester", message: "more" });
    clock.advanceBy(10_000);
    expect(overruns[0]).toMatchObject({ reminder: 1, budgetMs: 10_000 });
  });
});

describe("subagent tool — timeout_ms description", () => {
  it("says the budget does not stop the subagent and names the overrun wake", () => {
    const tool = createSubagentTool({
      getRegistry: () => null,
      getNotifyCenter: () => null,
      budgetMs: () => 45_000,
      defaultTimeoutMs: 1_800_000,
      defaultConcurrency: 4,
    });
    const description = (tool.parameters as unknown as { properties: { timeout_ms: { description: string } } }).properties
      .timeout_ms.description;
    expect(description).toContain("does not stop the subagent");
    expect(description).toContain('kind="subagent-overrun"');
    expect(description).not.toMatch(/hard timeout/i);
  });
});

describe("subagent tool — resume lifecycle", () => {
  // Contract that holds whether or not resume waits for its slot inside the
  // tool call: a resumed child gets a slot once one frees, runs, settles
  // once, and each settle produces exactly one wake.
  it("a resume made while every slot is busy runs once a slot frees and settles with one wake", async () => {
    const { exec, factory, notify, registry, clock } = makeStack({ autoComplete: null, maxConcurrentChildren: 1 });
    const first = await exec({ tasks: [{ prompt: "a", name: "alpha" }], async: true });
    const runA = (first.details as { run_id: string }).run_id;
    await flushMicrotasks();
    factory.sessions[0].complete("alpha done");
    await flushMicrotasks();
    expect(notify).toHaveBeenCalledTimes(1); // run A done

    const second = await exec({ tasks: [{ prompt: "b", name: "beta" }], async: true });
    const runB = (second.details as { run_id: string }).run_id;
    await flushMicrotasks();
    expect(registry.get(runB)!.children[0].status).toBe("running"); // holds the only slot

    const resumed = exec({ action: "resume", run_id: runA, child_id: "alpha", message: "more" });
    await flushMicrotasks();
    expect(factory.sessions[0].prompts).toEqual(["a"]); // no slot yet

    factory.sessions[1].complete("beta done"); // frees the slot
    await resumed;
    await flushMicrotasks();
    expect(factory.sessions[0].prompts).toEqual(["a", "more"]);
    expect(registry.get(runA)!.children[0].status).toBe("running");

    factory.sessions[0].complete("alpha again");
    await flushMicrotasks();
    expect(registry.get(runA)!.children[0].status).toBe("completed");
    const wakes = notify.mock.calls.map((c) => c[0].details as { kind: string; runId: string });
    expect(wakes.map((w) => `${w.kind} ${w.runId}`)).toEqual([
      `subagent-done ${runA}`,
      `subagent-done ${runB}`,
      `subagent-done ${runA}`,
    ]);
    expect(notify.mock.calls[2][0].content).toContain("alpha again");
    expect(clock.pendingTimers).toBe(0);
  });

  it("run-level subagent-done waits for a resumed child that is still running", async () => {
    const { exec, factory, notify, registry } = makeStack({ autoComplete: null });
    const started = await exec({ tasks: [{ prompt: "a", name: "alpha" }, { prompt: "b", name: "beta" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    factory.sessions[0].complete("alpha done"); // beta still running → handover
    await flushMicrotasks();
    await exec({ action: "resume", run_id: runId, child_id: "alpha", message: "more" });
    await flushMicrotasks();

    factory.sessions[1].complete("beta done"); // alpha (resumed) still running
    await flushMicrotasks();
    const kinds = () => notify.mock.calls.map((c) => (c[0].details as { kind: string }).kind);
    expect(kinds()).toEqual(["subagent-handover", "subagent-handover"]);
    expect(registry.get(runId)!.children.map((c) => c.status)).toEqual(["running", "completed"]);

    factory.sessions[0].complete("alpha again");
    await flushMicrotasks();
    expect(kinds()).toEqual(["subagent-handover", "subagent-handover", "subagent-done"]);
    const done = notify.mock.calls[2][0].details as { children: { name: string; status: string; result: string }[] };
    expect(done.children.map((c) => `${c.name} ${c.status}`)).toEqual(["alpha completed", "beta completed"]);
  });
});

describe("subagent tool — resume never waits for an admission slot", () => {
  /** Run A's only child has finished; run B's child holds the only slot. */
  async function fullQueue() {
    const stack = makeStack({ autoComplete: null, maxConcurrentChildren: 1 });
    const a = await stack.exec({ tasks: [{ prompt: "a", name: "alpha" }], async: true });
    const runA = (a.details as { run_id: string }).run_id;
    await flushMicrotasks();
    stack.factory.sessions[0].complete("alpha done");
    await flushMicrotasks();
    const b = await stack.exec({ tasks: [{ prompt: "b", name: "beta" }], async: true });
    const runB = (b.details as { run_id: string }).run_id;
    await flushMicrotasks();
    stack.notify.mockClear();
    return { ...stack, runA, runB };
  }

  /** Start a tool call and report whether it returned within a microtask flush. */
  async function returnsPromptly(call: Promise<unknown>): Promise<{ returned: boolean; value?: unknown }> {
    let out: { returned: boolean; value?: unknown } = { returned: false };
    void call.then((value) => {
      out = { returned: true, value };
    });
    await flushMicrotasks();
    return out;
  }

  it("the parent's resume call returns while every slot is busy; the child shows queued", async () => {
    const { exec, registry, factory, runA } = await fullQueue();
    const res = await returnsPromptly(exec({ action: "resume", run_id: runA, child_id: "alpha", message: "more" }));
    expect(res.returned).toBe(true);
    const value = res.value as { content: { text: string }[]; details: { queued_behind: number | null } };
    expect(value.details.queued_behind).toBe(0);
    expect(value.content[0].text).toContain(
      "Every subagent slot is busy, so it is queued and starts when a slot frees. You will be notified via",
    );
    expect(registry.get(runA)!.children[0].status).toBe("pending");
    expect(registry.get(runA)!.status).toBe("running");
    expect(factory.sessions[0].prompts).toEqual(["a"]); // admission still gates the turn
    await expect(exec({ action: "resume", run_id: runA, child_id: "alpha", message: "again" })).rejects.toThrow(
      "is already queued and starts when a subagent slot frees; its result arrives as a wake when it finishes. Do not resume it again.",
    );
  });

  it("a second queued resume reports how many are ahead of it", async () => {
    const { exec, factory, runA } = await fullQueue();
    // A third run whose child is also finished.
    const c = await exec({ tasks: [{ prompt: "c", name: "gamma" }], async: true });
    const runC = (c.details as { run_id: string }).run_id;
    await flushMicrotasks(); // queued behind beta: no session yet
    expect(factory.sessions).toHaveLength(2);
    const first = await exec({ action: "resume", run_id: runA, child_id: "alpha", message: "more" });
    expect((first.details as { queued_behind: number }).queued_behind).toBe(1); // gamma's launch is ahead
    expect(first.content[0].type === "text" && first.content[0].text).toContain(
      "Every subagent slot is busy and 1 subagent(s) are waiting ahead of it, so it is queued",
    );
    expect(runC).toMatch(/^run_/);
  });

  it("interrupting a queued resume settles it once and it never runs", async () => {
    const { exec, registry, factory, notify, runA, runB } = await fullQueue();
    const seen: string[] = [];
    registry.onTransition((run) => {
      if (run.runId === runA) seen.push(run.children[0].status);
    });
    await exec({ action: "resume", run_id: runA, child_id: "alpha", message: "more" });
    await exec({ action: "interrupt", run_id: runA, child_id: "alpha" });
    await flushMicrotasks();
    expect(registry.get(runA)!.children[0]).toMatchObject({ status: "interrupted" });
    expect(registry.get(runA)!.children[0].result?.text).toBe("");

    factory.sessions[1].complete("beta done"); // the slot reaches alpha's stale admission
    await flushMicrotasks();
    expect(factory.sessions[0].prompts).toEqual(["a"]);
    expect(seen).not.toContain("running");
    const wakes = notify.mock.calls.map((call) => {
      const d = call[0].details as { kind: string; runId: string };
      return `${d.kind} ${d.runId}`;
    });
    expect(wakes).toEqual([`subagent-done ${runA}`, `subagent-done ${runB}`]);

    // The slot was handed on, not leaked: a new launch gets it at once.
    await exec({ tasks: [{ prompt: "d", name: "delta" }], async: true });
    await flushMicrotasks();
    expect(factory.sessions.map((s) => s.prompts[0])).toEqual(["a", "b", "d"]);
  });

  it("a dead session found after the call returned is reported by a wake, not a tool error", async () => {
    const stack = makeStack({ autoComplete: null, hardTimeoutMs: 1_000 });
    stack.factory.configure = (session) => {
      session.hungAbort = true;
    };
    const started = await stack.exec({ tasks: [{ prompt: "a", name: "alpha" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await flushMicrotasks();
    stack.clock.advanceBy(1_000); // hard ceiling: interrupted, abort never finishes
    await flushMicrotasks();
    stack.notify.mockClear();

    const res = await stack.exec({ action: "resume", run_id: runId, child_id: "alpha", message: "more" });
    expect(res.content[0].type === "text" && res.content[0].text).toContain("alpha");
    stack.clock.advanceBy(0); // drain bound (stallMs 0 in this stack) expires
    await flushMicrotasks();
    expect(stack.notify).toHaveBeenCalledTimes(1);
    const wake = stack.notify.mock.calls[0][0].details as { kind: string; children: { status: string; error?: string }[] };
    expect(wake.kind).toBe("subagent-done");
    expect(wake.children[0].status).toBe("failed");
    expect(wake.children[0].error).toMatch(/did not go idle/);
    expect(stack.clock.pendingTimers).toBe(0);
  });
});
