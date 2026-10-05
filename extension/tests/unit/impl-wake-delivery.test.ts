import { describe, expect, it } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import { formatSubagentNotification } from "../../src/format";
import { NotifyCenter } from "../../src/notify";
import { SubagentRegistry } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import { createSubagentTool } from "../../src/subagent/tool";
import { logWakeInjected, onWakeMessageEnd, registryStatusLookup, wakeAtInjection } from "../../src/wake-delivery";
import { FAMULUS_WAKE_LEAD_IN, stampWakeAsOf } from "../../src/wake";
import { SessionFactory, tick } from "./subagent-fakes";

const T0 = Date.UTC(2026, 9, 5, 12, 27, 26, 61);

/** The custom message pi passes to message_end for a wake sent at `asOf`. */
function injected(asOf: number) {
  const wake = formatSubagentNotification({
    runId: "run_d8509ea2",
    status: "interrupted",
    durationMs: 1_800_000,
    children: [{ childId: "ch_39d879b7", name: "wave2-mails", status: "interrupted", text: "", error: "timeout" }],
  });
  const stamped = stampWakeAsOf({ customType: wake.customType, content: wake.content, details: wake.details }, asOf);
  return { role: "custom", ...stamped, display: true, timestamp: asOf };
}

describe("wake.inject: when a wake enters the model's context", () => {
  it("logs the enqueue → inject lag", () => {
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    // 800.9 s: the 2026-10-05 subagent-done that waited behind a blocked resume.
    logWakeInjected(injected(T0), { now: () => T0 + 800_882, logEvent: (type, fields) => events.push({ type, fields }) });
    expect(events).toEqual([
      {
        type: "wake.inject",
        fields: { kind: "subagent-done", ids: ["ch_39d879b7"], as_of: T0, lag_ms: 800_882 },
      },
    ]);
  });

  it("ignores everything that is not a pi-famulus wake", () => {
    const events: unknown[] = [];
    const deps = { now: () => T0, logEvent: (type: string) => events.push(type) };
    logWakeInjected({ role: "assistant", content: [] }, deps);
    logWakeInjected({ role: "custom", customType: "agent-intercom-cross-session", content: "x", details: { kind: "x" } }, deps);
    logWakeInjected({ role: "user", customType: "pi-famulus-wake", content: "x", details: { kind: "task" } }, deps);
    logWakeInjected(undefined, deps);
    expect(events).toEqual([]);
  });

  it("an unstamped wake (older sender) is logged without a lag", () => {
    const events: { type: string; fields?: Record<string, unknown> }[] = [];
    const message = { ...injected(T0), details: { kind: "supervisor-update", from: "ch_a", name: "a", message: "hi" } };
    logWakeInjected(message, { now: () => T0, logEvent: (type, fields) => events.push({ type, fields }) });
    expect(events).toEqual([{ type: "wake.inject", fields: { kind: "supervisor-update", ids: ["ch_a"] } }]);
  });
});

describe("age-ms: how old the wake is when the model sees it", () => {
  it("adds age-ms after as-of on the root, in content and details; nothing else changes", () => {
    const message = injected(T0);
    const out = wakeAtInjection(message, T0 + 800_882)!;
    expect(out.role).toBe("custom");
    expect(out.content).toBe(
      message.content.replace('as-of="2026-10-05T12:27:26Z">', 'as-of="2026-10-05T12:27:26Z" age-ms="800882">'),
    );
    expect(out.content.startsWith(FAMULUS_WAKE_LEAD_IN)).toBe(true);
    expect(out.details).toMatchObject({ kind: "subagent-done", asOf: T0, ageMs: 800_882 });
  });

  it("leaves content it did not render alone (not canonical, unstamped, not a wake)", () => {
    const tampered = { ...injected(T0) };
    tampered.content = tampered.content.replace("<summary>", "<summary>edited ");
    expect(wakeAtInjection(tampered, T0 + 5)).toBeUndefined();
    const unstamped = { ...injected(T0), details: { ...injected(T0).details, asOf: undefined } };
    expect(wakeAtInjection(unstamped, T0 + 5)).toBeUndefined();
    expect(wakeAtInjection({ role: "assistant", content: "x" }, T0)).toBeUndefined();
  });

  it("the message_end handler logs and returns the replacement", () => {
    const events: string[] = [];
    const out = onWakeMessageEnd(injected(T0), { now: () => T0 + 2_000, logEvent: (type) => events.push(type) });
    expect(events).toEqual(["wake.inject"]);
    expect(out?.content).toContain('age-ms="2000"');
  });

  it("a throwing logEvent does not break message_end", () => {
    expect(
      onWakeMessageEnd(injected(T0), {
        now: () => T0 + 2_000,
        logEvent: () => {
          throw new Error("log failed");
        },
      }),
    ).toBeUndefined();
  });
});

describe("subagent wakes re-check statuses when the model sees them", () => {
  /**
   * The 2026-10-05 shape, scaled down: a run-level subagent-done is queued
   * behind a busy parent; before it reaches the model, the parent resumes
   * one child (gets a slot) and another (queued). Real registry, tool and
   * NotifyCenter; pi's message_end is played by onWakeMessageEnd.
   */
  async function staleDone() {
    const clock = new ManualClock(T0);
    const registry = new SubagentRegistry({ clock, maxConcurrentChildren: 1 });
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const runner = new InProcessRunner({
      createSession: factory.fn,
      clock,
      stallMs: 0,
      acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
    });
    registry.setRunner(runner);
    const sent: { customType: string; content: string; details?: unknown }[] = [];
    const center = new NotifyCenter({ sendMessage: (m) => sent.push(m), isIdle: () => false, clock });
    const tool = createSubagentTool({
      getRegistry: () => registry,
      getNotifyCenter: () => center,
      budgetMs: () => 45_000,
      defaultTimeoutMs: 600_000,
      defaultConcurrency: 4,
      clock,
    });
    const exec = (params: Record<string, unknown>) =>
      tool.execute("tc", params as never, undefined, undefined, { cwd: "/tmp" } as ExtensionToolContext);
    const started = await exec({ tasks: [{ prompt: "a", name: "alpha" }, { prompt: "b", name: "beta" }], async: true });
    const runId = (started.details as { run_id: string }).run_id;
    await tick();
    factory.sessions[0].complete("alpha done"); // beta still queued → handover for alpha
    await tick();
    factory.sessions[1].complete("beta done"); // run-level subagent-done
    await tick();
    expect(sent.map((m) => (m.details as { kind: string }).kind)).toEqual(["subagent-handover", "subagent-done"]);

    clock.advanceBy(800_000); // the parent is busy elsewhere meanwhile
    await exec({ action: "resume", run_id: runId, child_id: "alpha", message: "more" }); // gets the slot
    await exec({ action: "resume", run_id: runId, child_id: "beta", message: "more" }); // queued
    await tick();
    const lookup = registryStatusLookup(registry);
    const inject = (m: (typeof sent)[number]) =>
      onWakeMessageEnd({ role: "custom", ...m }, { now: () => clock.now(), lookup: () => lookup });
    const childIds = registry.get(runId)!.children.map((c) => c.childId);
    return { sent, inject, runId, childIds, clock };
  }

  it("a run-level subagent-done shows current statuses, the snapshot's, and that the run is active again", async () => {
    const { sent, inject, childIds } = await staleDone();
    const [alpha, beta] = childIds;
    const out = inject(sent[1])!;
    expect(out.content).toContain('status="completed"');
    expect(out.content).toMatch(/status-now="running" as-of="2026-10-05T12:27:26Z" age-ms="800000">/);
    expect(out.content).toContain(
      `<changed-since-as-of>alpha (${alpha}): completed → running; beta (${beta}): completed → pending. ` +
        "The run is active again; another subagent-done arrives when it finishes.</changed-since-as-of>",
    );
    expect(out.content).toContain(`<child id="${alpha}" name="alpha" status="running" status-as-of="completed">`);
    expect(out.content).toContain(`<child id="${beta}" name="beta" status="pending" status-as-of="completed">`);
    expect(out.content).toContain("<result>alpha done</result>"); // the snapshot's results are kept
    expect(out.details).toMatchObject({
      runStatusNow: "running",
      children: [
        { childId: alpha, status: "running", statusAsOf: "completed" },
        { childId: beta, status: "pending", statusAsOf: "completed" },
      ],
    });
  });

  it("a handover whose child was resumed before the model saw it says so", async () => {
    const { sent, inject, childIds } = await staleDone();
    const out = inject(sent[0])!;
    expect(out.content).toMatch(/name="alpha" status="running" status-as-of="completed" as-of="[^"]+" age-ms="\d+">/);
    expect(out.content).toContain(
      `<changed-since-as-of>alpha (${childIds[0]}): completed → running. Its result arrives as a new wake when it finishes.</changed-since-as-of>`,
    );
  });

  it("an unchanged wake only gains age-ms", async () => {
    const { sent } = await staleDone();
    const lookup = { childStatus: () => undefined, runStatus: () => undefined };
    const out = onWakeMessageEnd({ role: "custom", ...sent[1] }, { now: () => T0 + 1_000, lookup: () => lookup })!;
    expect(out.content).not.toContain("status-as-of");
    expect(out.content).not.toContain("changed-since-as-of");
    expect(out.content).not.toContain("status-now");
    expect(out.content).toBe(sent[1].content.replace('Z">', 'Z" age-ms="1000">'));
  });
});
