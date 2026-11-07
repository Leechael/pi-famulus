import { test } from "node:test";
import assert from "node:assert/strict";
import { firstWait, isDetachedLaunch, sleepsToWait } from "./wait-choice.ts";

const bash = (command: string) => ({ name: "bash", args: { command } });
const turn = (...toolCalls: { name: string; args: Record<string, unknown> }[]) => ({ toolCalls });

// Commands taken from the 2026-11-07 log-monitor session.
test("a sleep inside a status check is a wait; short settle sleeps are not", () => {
  assert.equal(sleepsToWait("cd ~/.local/share/probe-detect && sleep 45; wc -l /tmp/v3-*.jsonl"), true);
  assert.equal(sleepsToWait("while [ \"$(grep -c '::' f)\" = \"0\" ]; do sleep 5; done"), true);
  assert.equal(sleepsToWait("pkill -f fleet-watch-run.sh; sleep 1; pkill -f tail"), false);
});

test("detached launches are recognised; && and 2>&1 are not detaching", () => {
  assert.equal(isDetachedLaunch("(./fleet-watch-run.sh > fleet/run.out 2>&1 &) ; sleep 3"), true);
  assert.equal(isDetachedLaunch("nohup ./x.sh > out"), true);
  assert.equal(isDetachedLaunch("./x.sh &"), true);
  assert.equal(isDetachedLaunch("(./a.sh > a.txt 2>&1; echo done) & (cd b && ./b.sh)"), true);
  assert.equal(isDetachedLaunch("cd a && ./x.sh > out 2>&1"), false);
  assert.equal(isDetachedLaunch("./x.sh &> out"), false);
});

test("the first wait decides, in call order", () => {
  assert.equal(firstWait([turn(bash("ls")), turn({ name: "monitor", args: { command: "x", persistent: true } })]).choice, "persistent-monitor");
  assert.equal(firstWait([turn({ name: "monitor", args: { command: "x", timeout_ms: 900000 } })]).detail, "timeout_ms=900000");
  assert.equal(firstWait([turn({ name: "monitor", args: { command: "x" } })]).choice, "timed-monitor");
  assert.equal(firstWait([turn(bash("sleep 55; date"), { name: "monitor", args: { command: "x", persistent: true } })]).choice, "sleep");
  assert.equal(firstWait([turn({ name: "task_output", args: { task_id: "sh_1" } })]).choice, "poll");
  assert.equal(firstWait([turn({ name: "task_output", args: { task_id: "sh_1", cursor: 10 } }), turn()]).choice, "end-turn");
  assert.equal(firstWait([turn(bash("ls"))]).choice, "none");
});

test("a detached producer before the monitor is flagged", () => {
  const r = firstWait([turn(bash("(./run.sh > out 2>&1 &)")), turn({ name: "monitor", args: { command: "tail -F out", persistent: true } })]);
  assert.deepEqual([r.choice, r.detached], ["persistent-monitor", true]);
});

test("turn zero alone decides; a launch with & and no wait is detached", async () => {
  const { decisionAtTurnZero } = await import("./wait-choice.ts");
  assert.equal(decisionAtTurnZero([turn(bash("(./run.sh > out 2>&1 &) ; sleep 2; echo started"))]).decision, "detached");
  assert.equal(decisionAtTurnZero([turn(bash("ls")), turn({ name: "monitor", args: { command: "x", persistent: true } })]).decision, "work");
  assert.equal(decisionAtTurnZero([turn()]).decision, "end-turn");
  assert.equal(decisionAtTurnZero([turn({ name: "monitor", args: { command: "./run.sh", persistent: true } })]).decision, "persistent-monitor");
});

test("after a guard refusal the next unrefused turn decides; other ways of waiting are evasion", async () => {
  const { decisionAfterGuard } = await import("./wait-choice.ts");
  const refused = { ...turn(bash("sleep 45; ls")), blocked: true };
  assert.equal(decisionAfterGuard([refused, turn({ name: "monitor", args: { command: "x", persistent: true } })]).decision, "persistent-monitor");
  assert.equal(decisionAfterGuard([refused, turn(bash("ls")), turn()]).decision, "end-turn");
  assert.equal(decisionAfterGuard([refused, turn(bash("sleep 9; ls"))]).decision, "sleep");
  assert.equal(decisionAfterGuard([refused, turn(bash("python3 -c 'import time; time.sleep(60)'"))]).decision, "evasion");
  assert.equal(decisionAfterGuard([refused, refused]).decision, "none");
  assert.equal(decisionAfterGuard([turn(bash("ls"))]).decision, "work");
});

test("with a persistent default, a monitor without timeout_ms is persistent", () => {
  const bare = [turn({ name: "monitor", args: { command: "x" } })];
  assert.equal(firstWait(bare).choice, "timed-monitor");
  assert.equal(firstWait(bare, { defaultPersistent: true }).choice, "persistent-monitor");
  assert.equal(firstWait([turn({ name: "monitor", args: { command: "x", timeout_ms: 60000 } })], { defaultPersistent: true }).choice, "timed-monitor");
  assert.equal(firstWait([turn({ name: "monitor", args: { command: "x", persistent: false } })], { defaultPersistent: true }).choice, "timed-monitor");
});
