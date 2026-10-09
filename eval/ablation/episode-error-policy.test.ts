/** Raw RPC fixtures: no subprocess, manager, provider, or credentials. */
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { RpcEvent } from "../lib/rpc.ts";
import { itemsFromEvents } from "../lib/transcript.ts";
import { errorAfterCompatibilityGrade, errorBeforeShutdown } from "./episode-error-policy.ts";
import type { Grade } from "./graders.ts";

const grade = (pass: boolean | null = false): Grade => ({ pass, reason: "fixture", metrics: {} });
const event = (type: string, extra: Record<string, unknown> = {}): RpcEvent => ({ type, seq: 0, t: 0, ...extra });
const assistant = (stopReason: string, errorMessage?: string) => event("message_end", {
  message: { role: "assistant", content: [], stopReason, ...(errorMessage ? { errorMessage } : {}) },
});
const pending = () => event("message_start", { message: { role: "assistant", content: [], stopReason: "pending" } });
const rpc = (...events: RpcEvent[]): RpcEvent[] => events.map((e, seq) => ({ ...e, seq, t: seq }));

describe("pre-shutdown episode error policy", () => {
  it("detects first-call and later-turn provider failures, including aborts", () => {
    for (const prefix of [[], [assistant("toolUse"), event("tool_execution_end")], [assistant("stop"), assistant("stop")]]) {
      for (const reason of ["error", "aborted"]) {
        assert.equal(errorBeforeShutdown(rpc(...prefix, assistant(reason, "unavailable")), grade()), "provider: unavailable");
      }
    }
    assert.equal(errorBeforeShutdown(rpc(assistant("error")), grade()), "provider: error");
  });

  it("does not let an otherwise passing grade hide a terminal provider error", () => {
    assert.equal(errorBeforeShutdown(rpc(assistant("stop"), assistant("error", "quota")), grade(true)), "provider: quota");
  });

  it("records recovered transient errors without poisoning a completed retry", () => {
    const g = grade(true);
    assert.equal(errorBeforeShutdown(rpc(assistant("error"), event("auto_retry_start"), assistant("error"), assistant("stop")), g), undefined);
    assert.equal(g.metrics.recoveredProviderErrors, 2);
  });

  it("retains a later unresolved error after an earlier recovered retry", () => {
    const g = grade();
    assert.equal(errorBeforeShutdown(rpc(assistant("error", "transient"), assistant("toolUse"), assistant("aborted", "interrupted")), g), "provider: interrupted");
    assert.equal(g.metrics.recoveredProviderErrors, 1);
  });

  it("classifies an unfinished response from raw events even when transcript has none", () => {
    for (const start of [event("turn_start"), pending()]) {
      const events = rpc(assistant("stop"), event("agent_start"), start);
      assert.equal(itemsFromEvents(events).length, 1, "normalization hides pending responses");
      for (const pass of [false, null]) {
        assert.match(errorBeforeShutdown(events, grade(pass)) ?? "", /pending assistant response/);
      }
    }
  });

  it("retains independently proven success at cutoff and records the pending response", () => {
    const g = grade(true);
    assert.equal(errorBeforeShutdown(rpc(assistant("stop"), pending()), g), undefined);
    assert.equal(g.metrics.cutoffPendingResponse, true);
  });

  it("does not confuse completed streaming or active tool execution with a stalled response", () => {
    for (const tail of [[event("agent_settled")], [event("tool_execution_start", { toolName: "bash" })]]) {
      const g = grade();
      assert.equal(errorBeforeShutdown(rpc(event("turn_start"), pending(), assistant("toolUse"), ...tail), g), undefined);
      assert.equal(g.metrics.cutoffPendingResponse, undefined);
    }
  });

  it("does not treat a non-assistant message_start as pending model output", () => {
    assert.equal(errorBeforeShutdown(rpc(assistant("stop"), event("message_start", { message: { role: "custom", content: "wake" } })), grade()), undefined);
  });

  it("ignores shutdown-emitted abort using an event-prefix snapshot, even at the same timestamp", () => {
    const events = rpc(assistant("stop"), event("agent_settled"));
    const beforeShutdown = events.slice();
    events.push({ ...assistant("aborted", "shutdown"), seq: 2, t: 1 });
    assert.equal(errorBeforeShutdown(beforeShutdown, grade(true)), undefined);
    assert.equal(errorBeforeShutdown(events, grade(true)), "provider: shutdown", "the real runner must pass only the captured prefix");
  });

  it("retains an existing setup/runtime error", () => {
    assert.equal(errorBeforeShutdown(rpc(assistant("error", "provider failure"), pending()), grade(), "pi not ready"), "pi not ready");
  });

  it("preserves proved compatibility misuse/tamper precedence with error provenance", () => {
    const scenario = { id: "monitor-waiter-event", optIn: true };
    const evidence: Grade["metrics"][] = [{ badWaiters: 1 }, { fixtureSourceIntact: false }];
    for (const metrics of evidence) {
      const g: Grade = { ...grade(), metrics };
      const error = errorBeforeShutdown(rpc(assistant("toolUse"), pending()), g);
      assert.match(error ?? "", /pending assistant response/);
      assert.equal(errorAfterCompatibilityGrade(scenario, g, error), undefined);
      assert.equal(g.metrics.episodeError, error);
    }
    const g = grade();
    const error = errorBeforeShutdown(rpc(assistant("stop"), assistant("error")), g);
    assert.equal(errorAfterCompatibilityGrade(scenario, g, error), "provider: error");
  });
});
