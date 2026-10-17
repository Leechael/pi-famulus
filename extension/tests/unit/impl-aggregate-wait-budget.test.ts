/**
 * An aggregate subagent wait (parallel tasks, or a chain) must return within
 * the configured foreground budget. A child that never finishes is reported
 * on that return, not left as a silent wait.
 */
import { describe, expect, it, vi } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import { SubagentRegistry } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import { createSubagentTool } from "../../src/subagent/tool";
import { SessionFactory, tick } from "./subagent-fakes";

const BUDGET_MS = 50;

function makeStack() {
  const clock = new ManualClock();
  const registry = new SubagentRegistry({ clock, maxConcurrentChildren: 4 });
  const factory = new SessionFactory();
  factory.autoComplete = "done";
  factory.configure = (session, req) => {
    if (req.name === "hangs" || req.name === "middle") session.autoComplete = null;
  };
  const runner = new InProcessRunner({
    createSession: factory.fn,
    clock,
    stallMs: 0,
    overrunRepeatMs: 60_000,
    acquire: async (req, ticket) => {
      const reservation = await registry.reserveChildSlot(req.childId, ticket);
      try {
        reservation.admit();
        return reservation.release;
      } catch (error) {
        reservation.release();
        throw error;
      }
    },
  });
  registry.setRunner(runner);
  const notify = vi.fn();
  const tool = createSubagentTool({
    getRegistry: () => registry,
    getNotifyCenter: () => ({ notify }),
    budgetMs: () => BUDGET_MS,
    defaultTimeoutMs: 600_000,
    defaultConcurrency: 4,
    clock,
  });
  const ctx = { cwd: "/tmp" } as ExtensionToolContext;
  const exec = (params: Record<string, unknown>) => tool.execute("tc", params as never, undefined, undefined, ctx);
  return { registry, factory, notify, exec, clock };
}

function textOf(result: { content: Array<{ type: string; text?: string }> }): string {
  return result.content.map((part) => part.text ?? "").join("\n");
}

/** Tool result plus any wake already handed to the parent when the call returns. */
function delivered(result: { content: Array<{ type: string; text?: string }>; details?: unknown }, notify: ReturnType<typeof vi.fn>): string {
  const wakes = notify.mock.calls.map((call) => {
    const message = call[0] as { content?: string };
    return message.content ?? "";
  });
  return [textOf(result), JSON.stringify(result.details ?? {}), ...wakes].join("\n");
}

async function settleWithinBudget(
  pending: Promise<{ content: Array<{ type: string; text?: string }>; details?: unknown }>,
  clock: ManualClock,
) {
  clock.advanceBy(BUDGET_MS);
  await tick();
  return Promise.race([
    pending,
    tick().then(() => {
      throw new Error("aggregate wait was still pending after the foreground budget elapsed");
    }),
  ]);
}

describe("aggregate subagent waits are bounded", () => {
  it("backgrounds parallel tasks and names the child that never finishes", async () => {
    const { exec, factory, notify, clock, registry } = makeStack();
    const pending = exec({
      tasks: [
        { prompt: "finish", name: "finishes" },
        { prompt: "block", name: "hangs" },
      ],
      concurrency: 2,
    });
    await tick();
    expect(factory.sessions).toHaveLength(2);

    const result = await settleWithinBudget(pending, clock);
    const details = result.details as { status?: string; run_id?: string };
    const child = registry.get(details.run_id ?? "")?.children.find((entry) => entry.name === "hangs");
    expect(details.status).toBe("backgrounded");
    expect(child?.status).toBe("running");
    expect(factory.sessions.find((session) => session.prompts.some((prompt) => prompt.includes("block")))?.isStreaming()).toBe(true);
    const report = delivered(result, notify);
    expect(report).toContain("hangs");
    expect(report).toMatch(/running|still/);
  });

  it("backgrounds a chain whose middle step never finishes and names that step", async () => {
    const { exec, factory, notify, clock, registry } = makeStack();
    const pending = exec({
      chain: [
        { prompt: "first", label: "first" },
        { prompt: "block", label: "middle" },
        { prompt: "last", label: "last" },
      ],
    });
    await tick();
    expect(factory.sessions.map((session) => session.prompts.some((prompt) => prompt.includes("block")))).toContain(true);
    expect(factory.sessions).toHaveLength(2);

    const result = await settleWithinBudget(pending, clock);
    const details = result.details as { status?: string; run_id?: string };
    const children = registry.get(details.run_id ?? "")?.children ?? [];
    expect(details.status).toBe("backgrounded");
    expect(children.find((entry) => entry.name === "middle")?.status).toBe("running");
    expect(children.find((entry) => entry.name === "last")?.status).toBe("pending");
    const report = delivered(result, notify);
    expect(report).toContain("middle");
    expect(report).toMatch(/running|still/);
  });
});
