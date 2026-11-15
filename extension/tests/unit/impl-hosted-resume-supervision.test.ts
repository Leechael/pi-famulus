/**
 * Regression tests for the 2026-11-15 parked-resume orphan (ch_2386fec0).
 *
 * A hosted child that was interrupted and parked is resumed with a new task.
 * The daemon respawns it and its cold `get_state` handshake reports `idle`
 * before the resumed prompt runs. That transport-ready idle used to resolve the
 * adapter's completion waiter, so the runner finished the new generation early
 * and reported the previous turn's abort error (or, without one, `completed`)
 * while the actual child kept working unsupervised.
 *
 * Production extension, registry, runner and HostedRpcSession run here; only
 * the daemon transport is fake, so the daemon's event order is replayed
 * exactly: the startup idle and RPC responses are emitted where the real
 * hosted.rs emits them.
 *
 * The invariant throughout: a terminal parent wake exists only after the
 * resumed turn's own `agent_start`/`agent_settled` lifecycle ended.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { agentRecordPath } from "../../src/subagent/agent-records";
import { resetHostedFakes, settleAsync, startHostedExtension, until, wakeDetails } from "./hosted-fakes";
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "famulus-hosted-resume-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(resetHostedFakes);

type Ext = Awaited<ReturnType<typeof startHostedExtension>>;
type DoneChild = { status?: string; error?: string; result?: string };

const STALE_ABORT = "This operation was aborted";

function record(sessionId: string, childId: string): { status?: string; error?: string } {
  return JSON.parse(readFileSync(agentRecordPath(home, sessionId, childId), "utf8"));
}

async function newExtension(sessionId: string): Promise<Ext> {
  return startHostedExtension({ sessionId, cwd: home });
}

function doneChildren(ext: Ext, since = 0): DoneChild[] {
  return wakeDetails(ext.pi.sent, "subagent-done")
    .slice(since)
    .flatMap((details) => (details.children ?? []) as DoneChild[]);
}

/**
 * One agent_input wrapper at a time: each stage replaces the previous hook.
 * `hook` runs after the call was recorded and may amend the RPC response
 * (used for prompt dispositions).
 */
function wrapInput(
  ext: Ext,
  hook: (op: string, childId: string) => Promise<Record<string, unknown> | void> | Record<string, unknown> | void,
): void {
  const input = ext.manager.agentInput.bind(ext.manager);
  ext.manager.agentInput = async (child, op, text, source) => {
    const response = await input(child, op, text, source);
    const patch = await hook(op, child);
    return patch ? { ...response, ...patch } : response;
  };
}

/**
 * The incident's first turn: started, interrupted, aborted, parked. Replays
 * the abort message and settle before the abort response, exactly as the
 * daemon forwards them.
 */
async function interruptedChild(ext: Ext): Promise<string> {
  const childId = await ext.spawnChild();
  ext.manager.rpc(childId, { type: "agent_start" });
  wrapInput(ext, async (op) => {
    if (op !== "abort") return;
    ext.manager.rpc(childId, {
      type: "message_end",
      message: { role: "assistant", content: [], stopReason: "aborted", errorMessage: STALE_ABORT },
    });
    ext.manager.rpc(childId, { type: "agent_settled", aborted: true });
    ext.manager.state(childId, "idle");
  });
  await ext.run({ action: "interrupt", child_id: childId });
  await settleAsync();
  // Parked, as after the incident's two-minute retention (or an explicit park).
  ext.manager.state(childId, "idle", "parked");
  return childId;
}

/** `agent_message send` to the interrupted child: the parent's resume path. */
async function resume(ext: Ext, childId: string, message = "Resume. New scope: PR B"): Promise<void> {
  await ext.pi.tool("agent_message").execute("resume", { action: "send", to: childId, message }, undefined, undefined, ext.ctx);
}

describe("resuming a parked hosted child keeps the new turn supervised", () => {
  it.each([false, true])("incident replay (parked=%s)", async (parked) => {
    const sessionId = `hosted-resume-${parked}`;
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      if (parked) {
        // ensure_warm() respawns the parked child; its get_state reports
        // isStreaming=false and the daemon publishes idle before dispatching.
        wrapInput(ext, async (op, child) => {
          if (op === "prompt") ext.manager.state(child, "idle", "warm");
        });
      }
      // Warm control: no respawn handshake, only the pre-fix behaviour to hold.
      await resume(ext, childId);
      await settleAsync();
      expect(doneChildren(ext, before), "no terminal wake from the startup idle").toEqual([]);
      expect(record(sessionId, childId).status).toBe("running");

      // The actual child accepted the prompt and started executing tools.
      ext.manager.rpc(childId, { type: "agent_start" });
      ext.manager.rpc(childId, {
        type: "tool_execution_start",
        toolCallId: "resumed-write",
        toolName: "write",
        args: { path: "guest_agent_mode.py" },
      });
      await settleAsync();
      expect(doneChildren(ext, before), "no terminal wake while the resumed child works").toEqual([]);
      expect(record(sessionId, childId).status).toBe("running");

      ext.manager.rpc(childId, { type: "tool_execution_end", toolCallId: "resumed-write", toolName: "write" });
      ext.manager.finish(childId, { content: [{ type: "text", text: "PR B finished" }], stopReason: "stop" });
      await until(() => doneChildren(ext, before).length === 1, "the resumed turn to complete");
      expect(doneChildren(ext, before)[0]).toMatchObject({ status: "completed" });
      await until(() => record(sessionId, childId).status === "completed", "the completed record");
    } finally {
      await ext.shutdown();
    }
  });

  it("does not report the previous turn's abort error for the resumed turn", async () => {
    const sessionId = "hosted-resume-stale-error";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      await resume(ext, childId);
      await settleAsync();
      expect(doneChildren(ext, before)).toEqual([]);

      ext.manager.rpc(childId, { type: "agent_start" });
      ext.manager.rpc(childId, { type: "message_end", message: {
        role: "assistant", content: [], stopReason: "error", errorMessage: "500 upstream overloaded",
      }});
      ext.manager.rpc(childId, { type: "agent_settled" });
      await until(() => doneChildren(ext, before).length === 1, "the resumed turn to fail");
      const child = doneChildren(ext, before)[0];
      expect(child.status).toBe("failed");
      expect(child.error).toContain("500 upstream overloaded");
      expect(child.error).not.toContain(STALE_ABORT);
    } finally {
      await ext.shutdown();
    }
  });

  it("treats a start after a settle as still running, not as completion", async () => {
    const sessionId = "hosted-resume-settle-fence";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      // A late settle from the drained generation must not close the new turn.
      wrapInput(ext, async (op, child) => {
        if (op === "prompt") ext.manager.rpc(child, { type: "agent_settled", aborted: true });
      });
      await resume(ext, childId);
      await settleAsync();
      ext.manager.rpc(childId, { type: "agent_start" });
      await settleAsync();
      expect(doneChildren(ext, before), "a stale agent_settled is not this turn's completion").toEqual([]);
      ext.manager.finish(childId, { content: [{ type: "text", text: "resumed" }], stopReason: "stop" });
      await until(() => doneChildren(ext, before).length === 1, "the resumed turn to complete");
      expect(doneChildren(ext, before)[0]).toMatchObject({ status: "completed" });
    } finally {
      await ext.shutdown();
    }
  });

  it("ignores a startup idle delivered after the resume ack", async () => {
    const sessionId = "hosted-resume-idle-after-ack";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      await resume(ext, childId);
      // Let the prompt RPC response through first: the respawn handshake's idle
      // snapshot can be delivered after the ack as well as before it.
      await settleAsync();
      ext.manager.state(childId, "idle", "warm");
      await settleAsync();
      expect(doneChildren(ext, before), "an idle after the ack is not completion").toEqual([]);
      expect(record(sessionId, childId).status).toBe("running");

      ext.manager.rpc(childId, { type: "agent_start" });
      ext.manager.rpc(childId, { type: "tool_execution_start", toolCallId: "late-idle-write", toolName: "write", args: { path: "late.md" } });
      await settleAsync();
      expect(doneChildren(ext, before)).toEqual([]);
      ext.manager.rpc(childId, { type: "tool_execution_end", toolCallId: "late-idle-write", toolName: "write" });
      ext.manager.finish(childId, { content: [{ type: "text", text: "late idle survived" }], stopReason: "stop" });
      await until(() => doneChildren(ext, before).length === 1, "the resumed turn to complete");
      expect(doneChildren(ext, before)[0]).toMatchObject({ status: "completed" });
    } finally {
      await ext.shutdown();
    }
  });

  it("waits for the joined run when the prompt is queued after its ack", async () => {
    const sessionId = "hosted-resume-queued-after-ack";
    const ext = await newExtension(sessionId);
    try {
      // A queued prompt joined a run that was already streaming: the settle of
      // that run is this prompt's completion.
      wrapInput(ext, async (op) => (op === "prompt" ? { disposition: "queued" } : undefined));
      const childId = await ext.spawnChild();
      await settleAsync();
      expect(doneChildren(ext), "queued is not completion").toEqual([]);
      expect(record(sessionId, childId).status).toBe("running");

      ext.manager.rpc(childId, { type: "agent_settled" });
      await until(() => doneChildren(ext).length === 1, "the queued turn to complete");
      expect(doneChildren(ext)).toHaveLength(1);
      expect(doneChildren(ext)[0]).toMatchObject({ status: "completed" });
    } finally {
      await ext.shutdown();
    }
  });

  it("completes once when a queued prompt's settle arrives before its ack", async () => {
    const ext = await newExtension("hosted-resume-queued-before-ack");
    try {
      wrapInput(ext, async (op, child) => {
        if (op !== "prompt") return;
        ext.manager.rpc(child, { type: "agent_settled" });
        return { disposition: "queued" };
      });
      await ext.spawnChild();
      await until(() => doneChildren(ext).length === 1, "the queued turn to complete");
      // Exactly one terminal result: the early settle must not resolve the
      // prompt again once the ack reports the disposition.
      expect(doneChildren(ext)).toHaveLength(1);
      expect(doneChildren(ext)[0]).toMatchObject({ status: "completed" });
    } finally {
      await ext.shutdown();
    }
  });

  it("completes a turn whose run settled before the prompt response arrived", async () => {
    const ext = await newExtension("hosted-resume-fast");
    try {
      // Fast completion: every lifecycle record can precede the RPC response.
      const input = ext.manager.agentInput.bind(ext.manager);
      ext.manager.agentInput = async (child, op, text, source) => {
        const response = await input(child, op, text, source);
        if (op === "prompt") {
          ext.manager.finish(child, { content: [{ type: "text", text: "finished before ack" }], stopReason: "stop" });
        }
        return response;
      };
      const childId = await ext.spawnChild();
      await until(() => wakeDetails(ext.pi.sent, "subagent-done").length === 1, "the fast turn to complete");
      expect(doneChildren(ext)[0]).toMatchObject({ status: "completed" });
      expect(childId).toBeTruthy();
    } finally {
      await ext.shutdown();
    }
  });

  it("completes a handled prompt after a finished turn without replaying its outcome", async () => {
    const sessionId = "hosted-resume-handled-after-turn";
    const ext = await newExtension(sessionId);
    try {
      const childId = await ext.spawnChild();
      ext.manager.finish(childId, { content: [{ type: "text", text: "first turn" }], stopReason: "stop" });
      await until(() => doneChildren(ext).length === 1, "the first turn");

      // An extension command/input handler consumed the next prompt: no run
      // follows, so the turn ends without waiting for agent_settled.
      wrapInput(ext, async (op) => (op === "prompt" ? { disposition: "handled" } : undefined));
      await resume(ext, childId, "handled by an extension command");
      await until(() => doneChildren(ext).length === 2, "the handled prompt to finish");
      const handled = doneChildren(ext)[1];
      // Status and error are turn-scoped; `result` remains the child's last
      // assistant text (session-scoped, so a stall retry keeps it).
      expect({ status: handled.status, error: handled.error }).toEqual({ status: "completed", error: undefined });
    } finally {
      await ext.shutdown();
    }
  });

  it("fails the resumed turn when the process dies after the prompt ack", async () => {
    const sessionId = "hosted-resume-process-failure-after-ack";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      await resume(ext, childId);
      // The prompt was accepted; the process then dies before its run starts.
      await settleAsync();
      ext.manager.state(childId, "failed", "none");
      await until(() => doneChildren(ext, before).length === 1, "the failed resumed turn");
      const child = doneChildren(ext, before)[0];
      expect(child.status).toBe("failed");
      expect(child.error).toContain("hosted child process failed");
      expect(child.error).not.toContain(STALE_ABORT);
    } finally {
      await ext.shutdown();
    }
  });

  it("holds a process failure reported before the prompt ack until it is accepted", async () => {
    const sessionId = "hosted-resume-process-failure-before-ack";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      // The failure reaches the parent while the dispatch RPC is still
      // outstanding; the success ack then must not wait for a settle.
      wrapInput(ext, async (op, child) => {
        if (op === "prompt") ext.manager.state(child, "failed", "none");
      });
      await resume(ext, childId);
      await until(() => doneChildren(ext, before).length === 1, "the failed resumed turn");
      const child = doneChildren(ext, before)[0];
      expect(child.status).toBe("failed");
      expect(child.error).toContain("hosted child process failed");
      expect(child.error).not.toContain(STALE_ABORT);
    } finally {
      await ext.shutdown();
    }
  });

  it("supervises a run the prompt was steered into (out-of-band work)", async () => {
    const sessionId = "hosted-resume-steered-run";
    const ext = await newExtension(sessionId);
    try {
      const childId = await ext.spawnChild();
      ext.manager.finish(childId, { content: [{ type: "text", text: "turn one" }], stopReason: "stop" });
      await until(() => doneChildren(ext).length === 1, "the first turn");
      const before = 1;

      // A user attach or a sibling child drives this hosted child: the runner
      // is idle, but the adapter sees a live run and no turn of its own.
      ext.manager.state(childId, "running");
      ext.manager.rpc(childId, { type: "agent_start" });
      await settleAsync();

      await resume(ext, childId, "Resume while the child is busy");
      await settleAsync();
      expect(ext.manager.inputs.at(-1)).toMatchObject({ op: "prompt_steer" });
      expect(doneChildren(ext, before), "the steered run is still executing").toEqual([]);
      expect(record(sessionId, childId).status).toBe("running");

      // The steered message and that run's remaining work end together.
      ext.manager.rpc(childId, { type: "message_end", message: {
        role: "assistant", content: [{ type: "text", text: "out-of-band done" }], stopReason: "stop",
      }});
      ext.manager.rpc(childId, { type: "agent_settled" });
      await until(() => doneChildren(ext, before).length === 1, "the steered turn to complete");
      expect(doneChildren(ext, before)[0]).toMatchObject({ status: "completed", result: "out-of-band done" });
    } finally {
      await ext.shutdown();
    }
  });

  it("fails the resumed turn when the dispatch RPC is rejected", async () => {
    const sessionId = "hosted-resume-dispatch-rejected";
    const ext = await newExtension(sessionId);
    try {
      const childId = await interruptedChild(ext);
      const before = wakeDetails(ext.pi.sent, "subagent-done").length;
      const input = ext.manager.agentInput.bind(ext.manager);
      ext.manager.agentInput = async (child, op, text, source) => {
        if (op === "prompt") {
          await input(child, op, text, source);
          throw new Error("hosted pi did not answer");
        }
        return input(child, op, text, source);
      };
      await resume(ext, childId);
      await until(() => doneChildren(ext, before).length === 1, "the rejected resumed turn");
      const child = doneChildren(ext, before)[0];
      expect(child.status).toBe("failed");
      expect(child.error).toContain("hosted pi did not answer");
    } finally {
      await ext.shutdown();
    }
  });
});
