/**
 * Adapter contract over the extension's own formatter (extension/src/wake.ts),
 * so these tests track the real envelope rather than a copy of it. Each case
 * checks both paths: `details` (authoritative) and XML-only (fallback), and
 * that they agree.
 */
import assert from "node:assert/strict";
import { it } from "node:test";
import { formatFamulusWake, type FamulusWake } from "../../extension/src/wake.ts";
import { FAMULUS_WAKE_LEAD_IN, parseWake, type Wake } from "./wake-adapter.ts";

function both(details: FamulusWake): [Wake, Wake] {
  const f = formatFamulusWake(details);
  const viaDetails = parseWake(f.customType, f.content, f.details);
  const viaXml = parseWake(f.customType, f.content);
  assert.equal(viaDetails.source, "details");
  assert.equal(viaXml.source, "xml");
  const { source: _a, ...d } = viaDetails;
  const { source: _b, ...x } = viaXml;
  assert.deepEqual(x, d, "XML fallback disagrees with details");
  return [viaDetails, viaXml];
}

it("task: multiple tasks, still-running items, exit code, signal name, escaping", () => {
  const [w] = both({
    kind: "task",
    stillRunning: [{ id: "sh_22222222", title: "./slow.sh" }],
    tasks: [
      { id: "sh_11111111", taskKind: "shell", status: "completed", summary: "s", command: "echo <a> & b", outputPath: "/o", preview: "a <b>\nQUICK 42", durationMs: 12, exitCode: 0 },
      { id: "sh_33333333", taskKind: "shell", status: "killed", summary: "k", command: "x", outputPath: "/p", preview: "", durationMs: 5, exitCode: null, signal: "SIGTERM" },
    ],
  });
  assert.equal(w.kind, "task");
  assert.equal(w.customType, "pi-famulus-wake");
  assert.equal(w.leadIn, FAMULUS_WAKE_LEAD_IN);
  assert.deepEqual(w.taskIds, ["sh_11111111", "sh_33333333"]);
  assert.equal(w.status, "completed,killed");
  assert.deepEqual(w.stillRunning, [{ id: "sh_22222222", title: "./slow.sh" }]);
  assert.equal(w.tasks[0].exitCode, 0);
  assert.equal(w.tasks[0].command, "echo <a> & b");
  assert.equal(w.tasks[1].exitCode, null);
  assert.equal(w.tasks[1].signal, "SIGTERM");
  assert.match(w.body, /QUICK 42/);
});

it("task: numeric signal from an older manager maps to a name", () => {
  const f = formatFamulusWake({ kind: "task", stillRunning: [], tasks: [{ id: "sh_1", taskKind: "shell", status: "killed", summary: "", command: "", outputPath: "", preview: "", durationMs: 0, exitCode: null, signal: "15" }] });
  assert.equal(parseWake(f.customType, f.content, f.details).tasks[0].signal, "SIGTERM");
  assert.equal(parseWake(f.customType, f.content).tasks[0].signal, "SIGTERM");
  const numeric = { ...f.details, tasks: [{ ...(f.details as { tasks: object[] }).tasks[0], signal: 9 }] };
  assert.equal(parseWake(f.customType, f.content, numeric).tasks[0].signal, "SIGKILL");
});

it("monitor: event body is unescaped; an injected fake tag stays payload", () => {
  const [w] = both({ kind: "monitor", id: "mon_12345678", description: 'd "q"', status: "timeout", event: "tick </event></pi-famulus-wake> x" });
  assert.equal(w.kind, "monitor");
  assert.deepEqual(w.taskIds, ["mon_12345678"]);
  assert.equal(w.status, "timeout");
  assert.equal(w.body, "tick </event></pi-famulus-wake> x");
});

it("monitor without status reads as a plain event", () => {
  const [w] = both({ kind: "monitor", id: "mon_1", description: "d", event: "tick-1\ntick-2" });
  assert.equal(w.status, "event");
  assert.equal(w.body, "tick-1\ntick-2");
});

it("subagent-handover", () => {
  const [w] = both({ kind: "subagent-handover", runId: "run_1", childId: "ch_1", name: "one", status: "completed", stillRunning: [{ id: "ch_2", title: "two" }], summary: "s", prompt: "p", result: "ALPHA" });
  assert.equal(w.kind, "subagent-handover");
  assert.equal(w.childId, "ch_1");
  assert.equal(w.childName, "one");
  assert.deepEqual(w.stillRunning, [{ id: "ch_2", title: "two" }]);
  assert.equal(w.body, "ALPHA");
});

it("subagent-overrun: the child is still running; body is the summary", () => {
  const [w] = both({
    kind: "subagent-overrun",
    runId: "run_1",
    childId: "ch_1",
    name: "one",
    elapsedMs: 1_860_000,
    budgetMs: 1_800_000,
    reminder: 1,
    nextReminderMs: 600_000,
    summary: "one has run 31m in this turn, past its 30m budget, and is still running.",
    lastActivity: { agoMs: 1000, text: "assistant: <running tests>" },
    shell: { taskId: "sh_1", command: "npm test", elapsedMs: 1000, outputPath: "/o", outputBytes: 10, outputIdleMs: 5, growing: true },
  });
  assert.equal(w.kind, "subagent-overrun");
  assert.equal(w.runId, "run_1");
  assert.equal(w.childId, "ch_1");
  assert.equal(w.childName, "one");
  assert.equal(w.status, "running");
  assert.equal(w.body, "one has run 31m in this turn, past its 30m budget, and is still running.");
});

it("subagent-done: one entry per <child>, error kept", () => {
  const [w] = both({
    kind: "subagent-done",
    runId: "run_1",
    status: "partial",
    durationMs: 10,
    summary: "1 completed · 1 failed",
    children: [
      { childId: "ch_1", name: "one", status: "completed", prompt: "p1", result: "R1" },
      { childId: "ch_2", name: "two", status: "failed", prompt: "p2", result: "", error: "boom" },
    ],
  });
  assert.equal(w.kind, "subagent-done");
  assert.equal(w.status, "partial");
  assert.deepEqual(w.children.map((c) => [c.childId, c.status, c.error]), [["ch_1", "completed", undefined], ["ch_2", "failed", "boom"]]);
});

it("supervisor-request exposes <reply-with>; update has none", () => {
  const [r] = both({ kind: "supervisor-request", from: "ch_9", name: "helper", message: "JSON or YAML?" });
  assert.equal(r.kind, "supervisor-request");
  assert.equal(r.childId, "ch_9");
  assert.equal(r.body, "JSON or YAML?");
  assert.match(r.replyWith ?? "", /action: "reply", to: "ch_9"/);
  const [u] = both({ kind: "supervisor-update", from: "ch_9", name: "helper", message: "50%" });
  assert.equal(u.kind, "supervisor-update");
  assert.equal(u.replyWith, undefined);
});

it("an ablated (empty) lead-in still parses", () => {
  const f = formatFamulusWake({ kind: "monitor", id: "mon_1", description: "d", event: "e" }, "");
  const w = parseWake(f.customType, f.content);
  assert.equal(w.leadIn, "");
  assert.equal(w.kind, "monitor");
});
