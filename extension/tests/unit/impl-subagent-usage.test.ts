import { describe, expect, it } from "vitest";
import { createPiUsageAdapter } from "../../src/subagent/pi-runtime";
import { InProcessRunner } from "../../src/subagent/runner";
import type { ChildRunRequest, ChildTokenUsage } from "../../src/subagent/types";
import { accumulateTokenUsage, normalizedTokenUsage } from "../../src/subagent/usage";
import { SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

describe("subagent token usage", () => {
  it("accumulates provider-reported message_end usage without counting missing fields", () => {
    let total = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: 120, output: 8 }));
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: 45, output: 13 }));
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: undefined, output: Number.NaN }));
    expect(total).toEqual({ input: 165, output: 21, cacheRead: 0, cacheWrite: 0 });
  });

  it("adapts cached usage into absolute message-end events across a resumed turn", () => {
    const adapter = createPiUsageAdapter();
    const recorded: Array<{ tokens_input: number; tokens_output: number; tokens_cache_read: number; tokens_cache_write: number }> = [];
    expect(adapter.adapt({ type: "tool_execution_start", toolCallId: "tool-1" }).toolCallId).toBe("tool-1");
    const sequence = [
      { type: "message_end", message: { role: "assistant", usage: { input: 100, output: 12, cacheRead: 80, cacheWrite: 4 } } },
      // The same adapter/session survives resume(); totals continue, events remain absolute.
      { type: "message_end", message: { role: "assistant", usage: { input: 50, output: 7, cacheRead: 30, cacheWrite: 2 } } },
    ];
    for (const event of sequence) {
      const adapted = adapter.adapt(event);
      expect(adapted.usage).toBeDefined();
      const total = adapter.tokenUsage();
      recorded.push({
        tokens_input: total.input,
        tokens_output: total.output,
        tokens_cache_read: total.cacheRead,
        tokens_cache_write: total.cacheWrite,
      });
    }
    expect(recorded).toEqual([
      { tokens_input: 100, tokens_output: 12, tokens_cache_read: 80, tokens_cache_write: 4 },
      { tokens_input: 150, tokens_output: 19, tokens_cache_read: 110, tokens_cache_write: 6 },
    ]);
    expect(adapter.tokenUsage()).toEqual({ input: 150, output: 19, cacheRead: 110, cacheWrite: 6 });
  });

  it("publishes absolute cache-aware usage snapshots across a resumed generation", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    type UsageEvent = {
      type: "agent.usage";
      child_id: string;
      tokens_input: number;
      tokens_output: number;
      tokens_cache_read: number;
      tokens_cache_write: number;
    };
    const usageEvents: UsageEvent[] = [];
    let handle: Awaited<ReturnType<InProcessRunner["start"]>> | undefined;
    const runner = new InProcessRunner({
      createSession: factory.fn,
      stallMs: 0,
      onActivity: (childId) => {
        const usage = handle?.tokenUsage();
        if (!usage) return;
        usageEvents.push({
          type: "agent.usage",
          child_id: childId,
          tokens_input: usage.input,
          tokens_output: usage.output,
          tokens_cache_read: usage.cacheRead,
          tokens_cache_write: usage.cacheWrite,
        });
      },
    });
    const request: ChildRunRequest = {
      childId: "ch_usage0001",
      runId: "run_usage0001",
      name: "worker",
      prompt: "initial turn",
      agent: WORKER_AGENT,
      timeoutMs: 60_000,
      depth: 1,
    };
    handle = await runner.start(request);
    const session = factory.sessions[0]!;
    const first: ChildTokenUsage = { input: 100, output: 12, cacheRead: 80, cacheWrite: 4 };
    session.setTokenUsage(first);
    session.emitEvent({ type: "message_end", role: "assistant", usage: first });
    expect(usageEvents.at(-1)).toEqual({
      type: "agent.usage",
      child_id: "ch_usage0001",
      tokens_input: 100,
      tokens_output: 12,
      tokens_cache_read: 80,
      tokens_cache_write: 4,
    });
    session.complete("generation A");
    await handle.result;

    await handle.resume("generation B");
    await tick();
    expect(factory.sessions).toHaveLength(1);
    const resumed: ChildTokenUsage = { input: 150, output: 19, cacheRead: 110, cacheWrite: 6 };
    session.setTokenUsage(resumed);
    session.emitEvent({ type: "message_end", role: "assistant", usage: { input: 50, output: 7, cacheRead: 30, cacheWrite: 2 } });
    expect(usageEvents.at(-1)).toEqual({
      type: "agent.usage",
      child_id: "ch_usage0001",
      tokens_input: 150,
      tokens_output: 19,
      tokens_cache_read: 110,
      tokens_cache_write: 6,
    });
    session.complete("generation B");
    await handle.result;
  });
});
