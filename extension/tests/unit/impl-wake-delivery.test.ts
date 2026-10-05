import { describe, expect, it } from "vitest";
import { formatSubagentNotification } from "../../src/format";
import { logWakeInjected } from "../../src/wake-delivery";
import { stampWakeAsOf } from "../../src/wake";

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
