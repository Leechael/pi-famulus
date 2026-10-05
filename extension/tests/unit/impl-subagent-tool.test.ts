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
  opts: { budgetMs?: number; autoComplete?: string | null; hardTimeoutMs?: number } = {},
) {
  const clock = new ManualClock();
  const registry = new SubagentRegistry({ clock });
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
