/**
 * Graders on hand-built transcripts, shaped after real batch-4 episodes
 * (2026-09-30) that the graders got wrong. Fast: no pi, no manager.
 *
 *   node --test ablation/scenarios.test.ts
 */
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import type { Item } from "../lib/transcript.ts";
import type { Wake } from "../lib/wake-adapter.ts";
import { firstWriteOf, isPoll } from "./graders.ts";
import { getScenario } from "./scenarios.ts";

const dirs: string[] = [];
after(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

function episode(files: Record<string, string> = {}, secrets: Record<string, string> = {}) {
  const root = mkdtempSync(join(tmpdir(), "pi-famulus-eval-grade-"));
  dirs.push(root);
  const cwd = join(root, "w");
  const secretDir = join(root, "secret");
  mkdirSync(cwd);
  mkdirSync(secretDir);
  for (const [name, text] of Object.entries(files)) writeFileSync(join(cwd, name), text);
  for (const [name, text] of Object.entries(secrets)) writeFileSync(join(secretDir, name), `${text}\n`);
  return { cwd, secretDir };
}

let seq = 0;
const at = (s: number) => ({ seq: seq++, t: s * 1000 });
const call = (s: number, id: string, name: string, args: Record<string, unknown>): Item => ({
  kind: "assistant",
  ...at(s),
  text: "",
  toolCalls: [{ id, name, args }],
});
const say = (s: number, text: string): Item => ({ kind: "assistant", ...at(s), text, toolCalls: [] });
const backgrounded = (s: number, callId: string, taskId: string): Item => ({
  kind: "toolResult",
  ...at(s),
  toolCallId: callId,
  toolName: "bash",
  text: `moved to background (task_id: ${taskId})`,
  details: { backgrounded: true, task_id: taskId },
  isError: false,
});
const taskWake = (s: number, taskId: string, status: string, body = ""): Item => ({
  kind: "wake",
  ...at(s),
  wake: { kind: "task", taskIds: [taskId], status, body, stillRunning: [], tasks: [{ id: taskId, status }], children: [] } as unknown as Wake,
});

describe("bg-end-turn with no wake", () => {
  const grade = getScenario("bg-end-turn").grade;

  // k3-256k, batch 2: its first model call took 71s, it backgrounded build.sh
  // at 73s and the 75s episode cap came before the 15s build could finish.
  it("is INVALID when the episode ended before the build could finish", () => {
    seq = 0;
    const items = [call(71, "c1", "bash", { command: "./build.sh" }), backgrounded(73, "c1", "sh_1"), say(74, "running")];
    const r = grade({ items, ...episode(), endedAt: 75_000 });
    assert.equal(r.pass, null, r.reason);
  });

  // A lost wake looks like this: the model ends its turn as told, and
  // nothing follows until the episode cap.
  it("is a FAIL when the build had time to finish and no wake came", () => {
    seq = 0;
    const items = [call(3, "c1", "bash", { command: "./build.sh" }), backgrounded(5, "c1", "sh_1"), say(6, "waiting")];
    const r = grade({ items, ...episode(), endedAt: 75_000 });
    assert.equal(r.pass, false);
    assert.match(r.reason, /no task wake/);
  });
});

describe("still-running-continue", () => {
  const scenario = getScenario("still-running-continue");

  // gpt-5.6-luna #6: both scripts in one command with shell redirects.
  it("one command running both scripts is INVALID, not a lost wake", () => {
    seq = 0;
    const items = [
      call(8, "c1", "bash", { command: './quick.sh > quick.txt & q=$!; ./slow.sh > slow.txt & s=$!; wait "$q"; wait "$s"' }),
      backgrounded(10, "c1", "sh_1"),
      say(12, "running both"),
    ];
    const r = scenario.grade({ items, ...episode() });
    assert.equal(r.pass, null, r.reason);
  });

  // grok-4.6 #0: slow.sh got a 5s timeout, was killed, and was restarted;
  // quick.txt was written at 12s, the restarted slow.sh finished at 52s.
  it("judges 'before slow' against the slow.sh run that completed", () => {
    seq = 0;
    const items = [
      call(5, "q", "bash", { command: "./quick.sh" }),
      call(5, "s1", "bash", { command: "./slow.sh", timeout: 5 }),
      backgrounded(6, "q", "sh_q"),
      backgrounded(6, "s1", "sh_s1"),
      taskWake(10, "sh_s1", "killed"),
      call(11, "s2", "bash", { command: "./slow.sh" }),
      backgrounded(11, "s2", "sh_s2"),
      taskWake(11, "sh_q", "completed", "QUICK Q1"),
      call(12, "w", "write", { path: "quick.txt", content: "QUICK Q1\n" }),
      taskWake(52, "sh_s2", "completed", "SLOW S1"),
      call(53, "w2", "write", { path: "slow.txt", content: "SLOW S1\n" }),
    ];
    const r = scenario.grade({ items, ...episode({ "quick.txt": "QUICK Q1\n" }, { q: "Q1", s: "S1" }) });
    assert.equal(r.pass, true, r.reason);
  });
});

// Batch 5 (2026-09-30).
describe("still-running-continue, batch 5", () => {
  const scenario = getScenario("still-running-continue");

  // k3-256k #0: `cat quick.sh slow.sh; ./quick.sh` names both scripts but runs
  // one. The first "one command runs both" rule called that INVALID.
  it("a command that only reads a script's source does not count as running it", () => {
    seq = 0;
    const items = [
      call(8, "q", "bash", { command: "cat quick.sh slow.sh; ./quick.sh" }),
      call(8, "s", "bash", { command: "./slow.sh" }),
      backgrounded(10, "q", "sh_q"),
      backgrounded(10, "s", "sh_s"),
      taskWake(17, "sh_q", "completed", "QUICK Q1"),
      call(23, "w", "bash", { command: "grep '^QUICK' out > quick.txt" }),
      taskWake(49, "sh_s", "completed", "SLOW S1"),
    ];
    const r = scenario.grade({ items, ...episode({ "quick.txt": "QUICK Q1\n" }, { q: "Q1", s: "S1" }) });
    assert.equal(r.pass, true, r.reason);
  });
});

describe("polling CLI detection", () => {
  it("recognises pi-famulus commands as polling", () => {
    for (const command of ["pi-famulus ls --json", "/tmp/isolated/bin/pi-famulus --home /tmp/h status --json"]) {
      assert.equal(isPoll({ id: "poll", name: "bash", args: { command } }), true, command);
    }
  });
});

describe("firstWriteOf", () => {
  // gpt-5.6-luna still-running #10 wrote quick.txt with mv; kimi-for-coding
  // handover #5 wrote a-result.txt with cp. Only > and tee were recognised.
  it("recognises mv and cp onto the file", () => {
    seq = 0;
    const mv = [call(9, "a", "bash", { command: 'tmp=$(mktemp quick.txt.XXXXXX); ./quick.sh > "$tmp" && mv "$tmp" quick.txt' })];
    assert.ok(firstWriteOf(mv, "quick.txt"));
    const cp = [call(23, "b", "bash", { command: "cp alpha.txt a-result.txt && od -c a-result.txt" })];
    assert.ok(firstWriteOf(cp, "a-result.txt"));
    const read = [call(1, "c", "bash", { command: "cp a-result.txt backup.txt" })];
    assert.equal(firstWriteOf(read, "a-result.txt"), undefined);
  });
});

describe("scenarios that were never exercised, batch 5", () => {
  // gpt-5.6-luna still-running #10: the backgrounded command wrote quick.txt
  // itself (mktemp + mv). The model never acted on a wake: nothing to grade.
  it("still-running: quick.txt written only by the quick command itself is INVALID", () => {
    seq = 0;
    const items = [
      call(9, "q", "bash", { command: 'tmp=$(mktemp quick.txt.XXXXXX); ./quick.sh > "$tmp" && mv "$tmp" quick.txt' }),
      call(9, "s", "bash", { command: 'tmp=$(mktemp slow.txt.XXXXXX); ./slow.sh > "$tmp" && mv "$tmp" slow.txt' }),
      backgrounded(11, "q", "sh_q"),
      backgrounded(11, "s", "sh_s"),
      taskWake(15, "sh_q", "completed", "QUICK Q1"),
      taskWake(50, "sh_s", "completed", "SLOW S1"),
    ];
    const r = getScenario("still-running-continue").grade({ items, ...episode({ "quick.txt": "QUICK Q1\n" }, { q: "Q1", s: "S1" }) });
    assert.equal(r.pass, null, r.reason);
  });

  // gpt-6-sol monitor #2: its first call came at 12s and its first look at
  // the log (15s) already showed READY. No waiting was ever needed.
  it("monitor: READY already in the log at the first look is INVALID", () => {
    seq = 0;
    const items: Item[] = [
      call(12, "a", "bash", { command: "ls -l service.log; pwd" }),
      { kind: "toolResult", ...at(12), toolCallId: "a", toolName: "bash", text: "service.log", details: undefined, isError: false },
      call(15, "b", "read", { path: "service.log" }),
      { kind: "toolResult", ...at(15), toolCallId: "b", toolName: "read", text: "starting service\nREADY token=R1\n", details: undefined, isError: false },
      say(17, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, null, r.reason);
  });

  it("monitor: looking before READY and again after it, with no wait, is still graded", () => {
    seq = 0;
    const items: Item[] = [
      call(3, "a", "read", { path: "service.log" }),
      { kind: "toolResult", ...at(3), toolCallId: "a", toolName: "read", text: "starting service\n", details: undefined, isError: false },
      call(16, "b", "read", { path: "service.log" }),
      { kind: "toolResult", ...at(16), toolCallId: "b", toolName: "read", text: "starting service\nREADY token=R1\n", details: undefined, isError: false },
      say(17, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, false, r.reason);
  });
});

// cubic review on #18 (2026-09-29/30).
describe("grader gaps from review", () => {
  const multiWake = (s: number, tasks: [string, string][]): Item => ({
    kind: "wake",
    ...at(s),
    wake: {
      kind: "task",
      taskIds: tasks.map(([id]) => id),
      status: tasks.map(([, st]) => st).join(","),
      tasks: tasks.map(([id, status]) => ({ id, status })),
      body: "",
      stillRunning: [],
      children: [],
    } as unknown as Wake,
  });
  const result = (s: number, id: string, name: string, text: string, isError = false): Item => ({
    kind: "toolResult",
    ...at(s),
    toolCallId: id,
    toolName: name,
    text,
    details: undefined,
    isError,
  });

  it("still-running: writing quick.txt only after slow.sh finished is a FAIL", () => {
    seq = 0;
    const items = [
      call(5, "q", "bash", { command: "./quick.sh" }),
      call(5, "s", "bash", { command: "./slow.sh" }),
      backgrounded(6, "q", "sh_q"),
      backgrounded(6, "s", "sh_s"),
      taskWake(12, "sh_q", "completed", "QUICK Q1"),
      say(13, "quick done; waiting for slow too"),
      taskWake(47, "sh_s", "completed", "SLOW S1"),
      call(48, "w", "write", { path: "quick.txt", content: "QUICK Q1\n" }),
    ];
    const r = getScenario("still-running-continue").grade({ items, ...episode({ "quick.txt": "QUICK Q1\n" }, { q: "Q1", s: "S1" }) });
    assert.equal(r.pass, false, r.reason);
    assert.match(r.reason, /waited for slow/);
  });

  it("still-running: slow.sh finishing in a wake shared with another task still counts", () => {
    seq = 0;
    const items = [
      call(5, "q", "bash", { command: "./quick.sh" }),
      call(5, "s", "bash", { command: "./slow.sh" }),
      call(5, "o", "bash", { command: "./other.sh" }),
      backgrounded(6, "q", "sh_q"),
      backgrounded(6, "s", "sh_s"),
      backgrounded(6, "o", "sh_o"),
      taskWake(12, "sh_q", "completed", "QUICK Q1"),
      say(13, "waiting"),
      multiWake(47, [["sh_o", "completed"], ["sh_s", "completed"]]),
      call(48, "w", "write", { path: "quick.txt", content: "QUICK Q1\n" }),
    ];
    const r = getScenario("still-running-continue").grade({ items, ...episode({ "quick.txt": "QUICK Q1\n" }, { q: "Q1", s: "S1" }) });
    assert.equal(r.pass, false, r.reason);
  });

  it("firstWriteOf: cp with a redirect after the destination writes; a quoted cp does not", () => {
    seq = 0;
    assert.ok(firstWriteOf([call(1, "a", "bash", { command: "cp alpha.txt a-result.txt 2>/dev/null" })], "a-result.txt"));
    assert.equal(firstWriteOf([call(1, "b", "bash", { command: 'echo "next: cp alpha.txt a-result.txt"' })], "a-result.txt"), undefined);
  });

  // 2026-10-07a: deepseek-flash wrote every result through the absolute cwd
  // path; 4 handover-continue episodes were false FAILs.
  it("firstWriteOf: a redirect, tee, or cp onto an absolute path writes; a longer file name does not", () => {
    seq = 0;
    const abs = "/private/tmp/eval-DJ880P/w/a-result.txt";
    assert.ok(firstWriteOf([call(1, "a", "bash", { command: `printf 'ALPHA-X\\n' > ${abs} && cat -A ${abs}` })], "a-result.txt"));
    assert.ok(firstWriteOf([call(1, "b", "bash", { command: `echo ALPHA-X | tee '${abs}'` })], "a-result.txt"));
    assert.ok(firstWriteOf([call(1, "c", "bash", { command: `cp alpha.txt ${abs}` })], "a-result.txt"));
    assert.equal(firstWriteOf([call(1, "d", "bash", { command: "echo x > /tmp/w/data-result.txt" })], "a-result.txt"), undefined);
  });

  // cubic suggested counting it; batch 5 has one such episode (grok-4.6 #9):
  // it read the log alongside arming, before any instruction, then waited.
  it("monitor: a log read in the same turn as arming the monitor is not a poll", () => {
    seq = 0;
    const items: Item[] = [
      {
        kind: "assistant",
        ...at(3),
        text: "",
        toolCalls: [
          { id: "m", name: "monitor", args: { command: "tail -F service.log | grep --line-buffered -m1 READY" } },
          { id: "r", name: "read", args: { path: "service.log" } },
        ],
      },
      result(3, "m", "monitor", "Monitor started"),
      result(3, "r", "read", "starting service\n"),
      say(4, "waiting"),
      { kind: "wake", ...at(15), wake: { kind: "monitor", taskIds: ["mon_1"], status: "event", body: "READY token=R1", tasks: [], stillRunning: [], children: [] } as unknown as Wake },
      say(16, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, true, r.reason);
  });

  it("monitor: a look in the same turn as the monitor that already shows READY is INVALID", () => {
    seq = 0;
    const items: Item[] = [
      {
        kind: "assistant",
        ...at(15),
        text: "",
        toolCalls: [
          { id: "m", name: "monitor", args: { command: "tail -F service.log | grep --line-buffered -m1 READY" } },
          { id: "r", name: "read", args: { path: "service.log" } },
        ],
      },
      result(15, "m", "monitor", "Monitor started"),
      result(15, "r", "read", "starting service\nREADY token=R1\n"),
      say(16, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, null, r.reason);
  });

  it("monitor: a successful quiet probe (grep -q) at the first look is INVALID", () => {
    seq = 0;
    const items: Item[] = [
      call(15, "g", "bash", { command: "grep -q READY service.log && echo found" }),
      result(15, "g", "bash", "found\n"),
      call(16, "m", "monitor", { command: "tail -F service.log | grep --line-buffered -m1 READY" }),
      result(16, "m", "monitor", "Monitor started"),
      { kind: "wake", ...at(17), wake: { kind: "monitor", taskIds: ["mon_1"], status: "event", body: "READY token=R1", tasks: [], stillRunning: [], children: [] } as unknown as Wake },
      say(18, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, null, r.reason);
  });
});
