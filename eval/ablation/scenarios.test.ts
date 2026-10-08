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
import { type Item, itemsFromEvents } from "../lib/transcript.ts";
import type { RpcEvent } from "../lib/rpc.ts";
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
const toolResult = (s: number, id: string, name: string, overrides: Partial<Extract<Item, { kind: "toolResult" }>> = {}): Item => ({
  kind: "toolResult", ...at(s), toolCallId: id, toolName: name,
  text: "ok", details: undefined, isError: false, ...overrides,
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

describe("still-running-continue delivery opportunities", () => {
  const scenario = getScenario("still-running-continue");
  const quickId = "call-a352f6d5-7e79-43d5-a83e-4b71bd76ae5c-0|fc_ad7d57d5-422b-9a11-880c-3118346e209c_0";
  const slowId = "call-a352f6d5-7e79-43d5-a83e-4b71bd76ae5c-1|fc_ad7d57d5-422b-9a11-880c-3118346e209c_1";
  const quickTask = {
    id: "sh_b9c2128d", taskKind: "shell", status: "completed", summary: 'Background command "./quick.sh" completed (exit code 0)',
    command: "./quick.sh", outputPath: "/tmp/eval-jhoS6q/h/sessions/01a11aaa-c248-76aa-b1b2-3f977ff1c0c9/tasks/sh_b9c2128d.output",
    preview: "QUICK 96F53FID\n", durationMs: 6424, exitCode: 0,
  };
  const slowTask = {
    id: "sh_56455a62", taskKind: "shell", status: "completed", summary: 'Background command "./slow.sh" completed (exit code 0)',
    command: "./slow.sh", outputPath: "/tmp/eval-jhoS6q/h/sessions/01a11aaa-c248-76aa-b1b2-3f977ff1c0c9/tasks/sh_56455a62.output",
    preview: "SLOW 0CUDDL31\n", durationMs: 40827, exitCode: 0,
  };
  // Actual RPC prefix projection: 2026-10-08c/baseline/xai_grok-4.7_high/
  // transcripts/xai_grok-4.7_high-baseline-still-running-continue-2.jsonl.
  // Preserve raw turn_start, seq/t, tool identities, args and authoritative wake
  // details; omit provider telemetry, rendered XML and tool-output boilerplate.
  const replayEvents = (): RpcEvent[] => [
    { type: "turn_start", seq: 3, t: 390 },
    { type: "message_end", seq: 9, t: 3266, message: { role: "assistant", content: [
      { type: "toolCall", id: quickId, name: "bash", arguments: { command: "./quick.sh" } },
      { type: "toolCall", id: slowId, name: "bash", arguments: { command: "./slow.sh" } },
    ] } },
    { type: "message_end", seq: 15, t: 5276, message: { role: "toolResult", toolCallId: quickId, toolName: "bash", content: [], details: { backgrounded: true, task_id: quickTask.id, fullOutputPath: quickTask.outputPath }, isError: false } },
    { type: "message_end", seq: 17, t: 5276, message: { role: "toolResult", toolCallId: slowId, toolName: "bash", content: [], details: { backgrounded: true, task_id: slowTask.id, fullOutputPath: slowTask.outputPath }, isError: false } },
    { type: "turn_start", seq: 19, t: 5277 },
    { type: "message_end", seq: 21, t: 60215, message: { role: "assistant", content: [{ type: "text", text: "Both scripts are running. I'll write `quick.txt` as soon as `quick.sh` finishes, without waiting for `slow.sh`." }] } },
    { type: "turn_start", seq: 23, t: 60216 },
    { type: "message_end", seq: 25, t: 60217, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: { kind: "task", stillRunning: [{ id: slowTask.id, title: "./slow.sh" }], tasks: [quickTask], asOf: 1791448770620, ageMs: 50318 } } },
    { type: "message_end", seq: 27, t: 60217, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: { kind: "task", stillRunning: [], tasks: [slowTask], asOf: 1791448805024, ageMs: 15915 } } },
    { type: "message_end", seq: 29, t: 65977, message: { role: "assistant", content: [
      { type: "text", text: "Both scripts finished. I'll write each output line to its result file now." },
      { type: "toolCall", id: "call-7641c317-aece-4bb0-9e13-6d030c2ddf71-2|fc_3384b08b-4b09-9a5f-9c5b-3e1c8595006c_0", name: "write", arguments: { path: "quick.txt", content: quickTask.preview } },
      { type: "toolCall", id: "call-7641c317-aece-4bb0-9e13-6d030c2ddf71-3|fc_3384b08b-4b09-9a5f-9c5b-3e1c8595006c_1", name: "write", arguments: { path: "slow.txt", content: slowTask.preview } },
    ] } },
  ];
  const ep = () => episode({ "quick.txt": "QUICK 96F53FID\n", "slow.txt": "SLOW 0CUDDL31\n" }, { q: "96F53FID", s: "0CUDDL31" });

  it("regression: a single coalesced quick-and-slow wake is INVALID even without turn metadata", () => {
    const events = replayEvents().filter((e) => e.type !== "turn_start" && e.seq !== 27).map((e) => e.seq === 25 ? {
      ...e, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: { kind: "task", tasks: [quickTask, slowTask], stillRunning: [] } },
    } : e);
    const r = scenario.grade({ items: itemsFromEvents(events), ...ep() });
    assert.equal(r.pass, null, r.reason);
    assert.match(r.reason, /quick and slow completions delivered together/);
  });

  it("invariant: unknown delivery turns are not inferred from adjacent wakes or equal timestamps", () => {
    const items = itemsFromEvents(replayEvents().filter((e) => e.type !== "turn_start"));
    const r = scenario.grade({ items, ...ep() });
    assert.equal(r.pass, false, r.reason);
    assert.match(r.reason, /waited for slow/);
  });

  it("invariant: distinct-turn waiting still fails, while writing quick before slow passes", () => {
    for (const response of ["ack", "no-assistant", "write"] as const) {
      // Synthetic split of the actual delivery batch. Same timestamps, but an
      // explicit new request for slow; lack of assistant text cannot excuse it.
      const events = replayEvents().flatMap((e): RpcEvent[] => {
        const scaled = { ...e, seq: e.seq * 10 };
        if (e.seq !== 25) return [scaled];
        const reply = { type: "message_end", seq: 251, t: 60217, message: { role: "assistant", content: response === "write"
          ? [{ type: "toolCall", id: "early-quick-write", name: "write", arguments: { path: "quick.txt", content: quickTask.preview } }]
          : [{ type: "text", text: "Quick finished; waiting for slow." }],
        } };
        return [scaled, ...(response === "no-assistant" ? [] : [reply]), { type: "turn_end", seq: 252, t: 60217 }, { type: "turn_start", seq: 253, t: 60217 }];
      });
      const r = scenario.grade({ items: itemsFromEvents(events), ...ep() });
      assert.equal(r.pass, response === "write", `${response}: ${r.reason}`);
      assert.equal(r.metrics.wroteQuickBeforeSlowWake, response === "write");
      assert.match(r.reason, response === "write" ? /continued while slow/ : /waited for slow/);
    }
  });

  it("regression: actual quick and slow wakes delivered in one request are INVALID", () => {
    const items = itemsFromEvents(replayEvents());
    const wakeItems = items.filter((i) => i.kind === "wake");
    assert.deepEqual(wakeItems.map((w) => [w.seq, w.turnSeq]), [[25, 23], [27, 23]]);
    const r = scenario.grade({ items, ...ep() });
    assert.equal(r.pass, null, r.reason);
    assert.match(r.reason, /quick and slow completions delivered together/);
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

describe("handover-continue finish order", () => {
  const scenario = getScenario("handover-continue");
  const alpha = "ALPHA-Z41NY7W8";
  // Minimized message_end projection of the actual RPC (not synthesized Items):
  // eval/results/2026-10-08a/baseline/xai_grok-4.7_medium/transcripts/
  // xai_grok-4.7_medium-baseline-handover-continue-2.jsonl.
  // Omit provider telemetry/reasoning; retain the consumed wake/result fields,
  // original IDs, seqs, times, and B write. Alpha is from the bash result at seq15.
  const runCallId = "call-bb0ebb32-fc4a-4857-8019-64692755d8d9-1|fc_3dc890dd-28b3-93d1-b2db-4fbabcc3d284_1";
  const writeCallId = "call-8a4d8a56-fea0-4ee4-a60b-7984d8ad6152-2|fc_271f976d-d6cf-9af2-92ab-d6a9f6443244_0";
  const replay = () => itemsFromEvents([
    { type: "message_end", seq: 9, t: 30092, message: { role: "assistant", content: [
      { type: "toolCall", id: runCallId, name: "subagent", arguments: { tasks: [
        { prompt: "Read alpha.txt and reply with its exact contents.", name: "alpha-reader", work_kind: "read/search" },
        { prompt: "Run ./slow-child.sh and reply with its exact output.", name: "slow-child", work_kind: "other" },
      ], async: true } },
    ] } },
    { type: "message_end", seq: 17, t: 30150, message: { role: "toolResult", toolCallId: runCallId, toolName: "subagent", content: [], details: { run_id: "run_0dfe9d96", status: "backgrounded" }, isError: false } },
    { type: "message_end", seq: 28, t: 70380, message: { role: "custom", customType: "pi-famulus-wake", content: 'System wake — not a new user message. Handle this <pi-famulus-wake> before other work.\n\n<pi-famulus-wake kind="subagent-handover" run-id="run_0dfe9d96" child-id="ch_0e9d0f48" name="slow-child" status="completed" as-of="2026-10-08T07:54:24Z" age-ms="2">\n  <still-running>\n    <item id="ch_d755fbe9">alpha-reader</item>\n  </still-running>\n  <summary>slow-child completed; 1 still running</summary>\n  <prompt>Run ./slow-child.sh and reply with its exact output.</prompt>\n  <result>BETA-DONE</result>\n</pi-famulus-wake>', details: {
      kind: "subagent-handover", runId: "run_0dfe9d96", childId: "ch_0e9d0f48", name: "slow-child", status: "completed",
      stillRunning: [{ id: "ch_d755fbe9", title: "alpha-reader" }], summary: "slow-child completed; 1 still running",
      prompt: "Run ./slow-child.sh and reply with its exact output.", result: "BETA-DONE", asOf: 1791446064802, ageMs: 2,
    } } },
    { type: "message_end", seq: 30, t: 83559, message: { role: "assistant", content: [
      { type: "text", text: "Task 2 finished first. I'll write its output now and keep waiting for task 1." },
      { type: "toolCall", id: writeCallId, name: "write", arguments: { path: "b-result.txt", content: "BETA-DONE\n" } },
    ] } },
    { type: "message_end", seq: 34, t: 83561, message: { role: "toolResult", toolCallId: writeCallId, toolName: "write", content: [{ type: "text", text: "Successfully wrote to b-result.txt" }], isError: false } },
    { type: "message_end", seq: 40, t: 88550, message: { role: "assistant", content: [{ type: "text", text: "`b-result.txt` has task 2's output. Still waiting on the alpha reader before writing `a-result.txt`." }] } },
  ]);

  it("regression: alpha handover and same-run done delivered in one request are INVALID", () => {
    for (const mode of ["same-turn", "distinct-turn", "early-write", "other-run", "unknown"] as const) {
      const writeMessage = { role: "assistant", content: [{ type: "toolCall", id: "write-a", name: "write", arguments: { path: "a-result.txt", content: `${alpha}\n` } }] };
      const events: RpcEvent[] = [
        { type: "turn_start", seq: 1, t: 1 },
        { type: "message_end", seq: 20, t: 100, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: {
          kind: "subagent-handover", runId: "run_0dfe9d96", childId: "ch_d755fbe9", name: "alpha-reader", status: "completed", result: alpha,
          stillRunning: [{ id: "ch_0e9d0f48", title: "slow-child" }],
        } } },
        ...(mode === "distinct-turn" || mode === "early-write" ? [
          { type: "message_end", seq: 21, t: 100, message: mode === "early-write" ? writeMessage : { role: "assistant", content: [{ type: "text", text: "A finished; waiting for B." }] } },
          { type: "turn_end", seq: 22, t: 100 },
          { type: "turn_start", seq: 30, t: 100 },
        ] : []),
        { type: "message_end", seq: 40, t: 100, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: {
          kind: "subagent-done", runId: mode === "other-run" ? "run_unrelated" : "run_0dfe9d96", status: "completed", children: [
            { childId: "ch_d755fbe9", name: "alpha-reader", status: "completed", prompt: "Read alpha.txt", result: alpha },
          ],
        } } },
        { type: "message_end", seq: 50, t: 101, message: writeMessage },
      ];
      const items = itemsFromEvents(events.filter((e) => mode !== "unknown" || e.type !== "turn_start"));
      const r = scenario.grade({ items, ...episode({ "a-result.txt": `${alpha}\n` }, { alpha }) });
      assert.equal(r.pass, mode === "same-turn" ? null : mode === "early-write" || mode === "other-run", `${mode}: ${r.reason}`);
      if (mode === "same-turn") assert.match(r.reason, /alpha handover and run completion delivered together/);
      if (mode === "distinct-turn" || mode === "unknown") assert.match(r.reason, /waited for the whole run/);
    }
  });

  it("regression: B's output alone must not stop an episode while A is still running", () => {
    const items = replay();
    const ep = { items, ...episode({ "alpha.txt": `${alpha}\n`, "b-result.txt": "BETA-DONE\n" }, { alpha }) };
    assert.equal(scenario.done(items, ep), false);
    writeFileSync(join(ep.cwd, "a-result.txt"), `${alpha}\n`);
    assert.equal(scenario.done(items, ep), true);
  });

  // Synthetic controls keep the real run/child identities but change completion
  // order explicitly. They are not claimed as incident-exact replay.
  const alphaWake = (kind: "subagent-handover" | "subagent-done", seq: number) => itemsFromEvents([
    { type: "message_end", seq, t: seq * 1000, message: { role: "custom", customType: "pi-famulus-wake", content: "", details: {
      kind, runId: "run_0dfe9d96", childId: "ch_d755fbe9", name: "alpha-reader", status: "completed", result: alpha,
      stillRunning: kind === "subagent-handover" ? [{ id: "ch_0e9d0f48", title: "slow-child" }] : [],
      children: [{ childId: "ch_d755fbe9", name: "alpha-reader", status: "completed", prompt: "Read alpha.txt and reply with its exact contents.", result: alpha }],
    } } },
  ]);

  it("invariant: a valid alpha-first handover still requires a correct write before run completion", () => {
    for (const mode of ["missing", "before", "after"] as const) {
      seq = mode === "after" ? 60 : 30;
      const items = [...alphaWake("subagent-handover", 28)];
      if (mode !== "missing") items.push(call(seq, "a-write", "write", { path: "a-result.txt", content: `${alpha}\n` }));
      items.push(...alphaWake("subagent-done", 50));
      items.sort((a, b) => a.seq - b.seq);
      const r = scenario.grade({ items, ...episode(mode === "missing" ? {} : { "a-result.txt": `${alpha}\n` }, { alpha }) });
      assert.equal(r.pass, mode === "before", `${mode}: ${r.reason}`);
      assert.equal(r.metrics.aOk, mode !== "missing");
      assert.equal(r.metrics.wroteABeforeRunDone, mode === "before");
      assert.match(r.reason, mode === "missing" ? /wrong\/missing/ : mode === "before" ? /continued from handover/ : /waited for the whole run/);
    }
  });

  it("invariant: alpha revealed only at run-done, or handed over after it, is INVALID", () => {
    for (const lateHandover of [false, true]) {
      const items = [...replay(), ...alphaWake("subagent-done", 50), ...(lateHandover ? alphaWake("subagent-handover", 60) : [])];
      const r = scenario.grade({ items, ...episode({ "a-result.txt": `${alpha}\n`, "b-result.txt": "BETA-DONE\n" }, { alpha }) });
      assert.equal(r.pass, null, r.reason);
      assert.match(r.reason, /no alpha-result handover before run completion/);
    }
  });

  it("regression: actual B-first handover is INVALID, not an unwritten-alpha failure", () => {
    const items = replay();
    const wake = items.find((i) => i.kind === "wake");
    assert.equal(wake?.wake.body, "BETA-DONE");
    assert.deepEqual(wake?.wake.stillRunning, [{ id: "ch_d755fbe9", title: "alpha-reader" }]);
    const r = scenario.grade({ items, ...episode({ "alpha.txt": `${alpha}\n`, "b-result.txt": "BETA-DONE\n" }, { alpha }) });
    assert.equal(r.pass, null, r.reason);
    assert.match(r.reason, /no alpha-result handover before run completion/);
  });
});

describe("monitor first-look regressions", () => {
  it("regression: actual metadata-only probe does not precede the first content look", () => {
    // Actual message_end projection (provider telemetry/reasoning omitted):
    // 2026-10-08a/baseline/xai_grok-4.7_high/transcripts/
    // xai_grok-4.7_high-baseline-monitor-not-sleep-0.jsonl.
    const probeId = "call-5127321e-da4e-45e8-8da5-0d9cdc123c08-0|fc_bb4a727c-6b0d-9571-aeda-ec50b8ccdc02_0";
    const readId = "call-2452ff3c-bbd8-4841-a94e-7014ead19440-1|fc_de7b57df-f5ed-957f-9ce1-b04d638eca86_0";
    const items = itemsFromEvents([
      { type: "message_end", seq: 9, t: 14077, message: { role: "assistant", content: [
        { type: "text", text: "I'll watch `service.log` for the READY line and report the token once it appears." },
        { type: "toolCall", id: probeId, name: "bash", arguments: { command: 'ls -la && (test -f service.log && wc -l service.log || echo "no service.log yet")' } },
      ] } },
      { type: "message_end", seq: 13, t: 14111, message: { role: "toolResult", toolCallId: probeId, toolName: "bash", content: [{ type: "text", text: "total 4\ndrwxr-xr-x 3 leechael wheel  96 Oct  8 15:55 .\ndrwx------ 6 leechael wheel 192 Oct  8 15:55 ..\n-rw-r--r-- 1 leechael wheel  17 Oct  8 15:55 service.log\n1 service.log\n" }], isError: false } },
      { type: "message_end", seq: 17, t: 16965, message: { role: "assistant", content: [{ type: "toolCall", id: readId, name: "read", arguments: { path: "/private/tmp/eval-qcY79g/w/service.log" } }] } },
      { type: "message_end", seq: 21, t: 16967, message: { role: "toolResult", toolCallId: readId, toolName: "read", content: [{ type: "text", text: "starting service\nREADY token=VL45NHA0\n" }], isError: false } },
      { type: "message_end", seq: 25, t: 20174, message: { role: "assistant", content: [{ type: "text", text: "The READY line is already in `service.log`. The token is `VL45NHA0`." }] } },
    ]);
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "VL45NHA0" }) });
    assert.equal(r.pass, null, r.reason);
    assert.match(r.reason, /already in the log at the first look/);
  });

  it("invariant: metadata probes are not content looks, but remain polls after arming", () => {
    for (const command of ["ls -l service.log", "test -f service.log", "[ -f service.log ]", "wc -l service.log", "stat service.log", "/usr/bin/file service.log"]) {
      seq = 0;
      const items: Item[] = [
        call(1, "probe", "bash", { command }),
        toolResult(1, "probe", "bash", { text: "service.log" }),
        call(16, "read", "read", { path: "service.log" }),
        toolResult(16, "read", "read", { text: "READY token=R1" }),
        say(17, "The token is R1."),
      ];
      const ep = episode({}, { ready: "R1" });
      const scenario = getScenario("monitor-not-sleep");
      assert.equal(scenario.grade({ items, ...ep }).pass, null, command);

      seq = 0;
      const monitored: Item[] = [
        call(1, "monitor", "monitor", { command: "tail -F service.log" }),
        toolResult(1, "monitor", "monitor", { text: "Monitor started" }),
        call(2, "probe", "bash", { command }),
        toolResult(2, "probe", "bash", { text: "service.log" }),
        { kind: "wake", ...at(15), wake: { kind: "monitor", taskIds: ["mon_1"], status: "event", body: "READY token=R1", tasks: [], stillRunning: [], children: [] } as unknown as Wake },
        say(16, "The token is R1."),
      ];
      const r = scenario.grade({ items: monitored, ...ep });
      assert.equal(r.pass, false, `${command}: ${r.reason}`);
      assert.equal(r.metrics.polls, 1);
      assert.match(r.reason, /polled 1x/);
    }
  });

  it("invariant: metadata-led compounds still recognize actual content inspection", () => {
    for (const command of ["wc -l service.log; cat service.log", "stat service.log && head service.log", "file service.log; tail service.log", "test -f service.log && grep READY service.log"]) {
      seq = 0;
      const items = [call(16, "look", "bash", { command }), toolResult(16, "look", "bash", { text: "READY token=R1" })];
      const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
      assert.equal(r.pass, null, `${command}: ${r.reason}`);
      assert.match(r.reason, /already in the log at the first look/);
    }
  });

  it("regression: an ls-led compound content read can show READY at the first look", () => {
    for (const command of ["ls -l service.log; cat service.log", "ls -la && grep READY service.log"]) {
      seq = 0;
      const items: Item[] = [
        call(15, "look", "bash", { command }),
        { kind: "toolResult", ...at(15), toolCallId: "look", toolName: "bash", text: "service.log\nstarting service\nREADY token=R1\n", details: undefined, isError: false },
        say(16, "The token is R1."),
      ];
      const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
      assert.equal(r.pass, null, `${command}: ${r.reason}`);
      assert.match(r.reason, /already in the log at the first look/);
    }
  });
});

describe("monitor execution credit", () => {
  it("regression: validation feedback mentioning READY is not a first look at the log", () => {
    seq = 0;
    const items: Item[] = [
      call(1, "look", "bash", { command: "grep READY service.log" }),
      toolResult(1, "look", "bash", { isError: true, text: 'Validation failed for tool "bash": invalid timeout. Received arguments: {"command":"grep READY service.log"}' }),
      call(2, "monitor", "monitor", { command: "tail -F service.log | grep --line-buffered READY" }),
      toolResult(2, "monitor", "monitor", { text: "Monitor started" }),
      { kind: "wake", ...at(15), wake: { kind: "monitor", taskIds: ["mon_1"], status: "event", body: "READY token=R1", tasks: [], stillRunning: [], children: [] } as unknown as Wake },
      say(16, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, true, r.reason);
    assert.equal(r.metrics.waitVia, "monitor");
    assert.equal(r.metrics.polls, 0);
  });

  it("invariant: a grep that executes but finds no READY remains the first look", () => {
    seq = 0;
    const items: Item[] = [
      call(1, "look", "bash", { command: "grep READY service.log" }),
      toolResult(1, "look", "bash", { isError: true, text: "Command exited with code 1" }),
      call(16, "read", "read", { path: "service.log" }),
      toolResult(16, "read", "read", { text: "READY token=R1" }),
      say(17, "The token is R1."),
    ];
    const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
    assert.equal(r.pass, false, r.reason);
    assert.equal(r.metrics.polls, 1);
  });

  it("invariant: a successful monitor retry earns credit but rejected polls remain attempts", () => {
    seq = 0;
    const items: Item[] = [
      call(1, "failed", "monitor", { command: "tail -F service.log" }),
      toolResult(1, "failed", "monitor", { isError: true, text: "Validation failed" }),
      call(2, "retry", "monitor", { command: "tail -F service.log" }),
      toolResult(2, "retry", "monitor", { text: "Monitor started" }),
      say(16, "The token is R1."),
    ];
    const ep = episode({}, { ready: "R1" });
    const scenario = getScenario("monitor-not-sleep");
    assert.equal(scenario.grade({ items, ...ep }).pass, true);
    items.push(call(17, "poll", "task_list", {}), toolResult(17, "poll", "task_list", { isError: true, text: "Validation failed" }));
    const polled = scenario.grade({ items, ...ep });
    assert.equal(polled.pass, false);
    assert.equal(polled.metrics.polls, 1);
    assert.match(polled.reason, /polled 1x/);
  });

  it("regression: rejected or unconfirmed waits do not earn event-driven credit", () => {
    for (const name of ["monitor", "bash"]) {
      for (const failure of ["isError", "ok-false", "missing"] as const) {
        seq = 0;
        const items: Item[] = [call(3, "wait", name, { command: "tail -F service.log | grep --line-buffered -m1 READY" })];
        if (failure !== "missing") items.push({
          kind: "toolResult", ...at(3), toolCallId: "wait", toolName: name,
          text: `Validation failed for tool "${name}": invalid arguments`,
          details: failure === "ok-false" ? { ok: false } : undefined,
          isError: failure === "isError",
        });
        items.push(say(16, "The token is R1."));
        const r = getScenario("monitor-not-sleep").grade({ items, ...episode({}, { ready: "R1" }) });
        assert.equal(r.pass, false, `${name}/${failure}: ${r.reason}`);
        assert.equal(r.metrics.waitVia, "none");
        assert.match(r.reason, /no event-driven wait/);
      }
    }
  });
});

describe("control action execution credit", () => {
  it("regression: overrun decisions require accepted actions, not rejected or missing results", () => {
    for (const scenarioId of ["overrun-stuck", "overrun-progressing"]) {
      for (const action of ["interrupt", "extend", "send"]) {
        for (const outcome of ["isError", "ok-false", "missing", "success", "retry"] as const) {
          seq = 0;
          const wake = (s: number): Item => ({ kind: "wake", ...at(s), wake: {
            kind: "subagent-overrun", raw: '<shell growing="yes">build.sh</shell>',
            taskIds: [], tasks: [], children: [], stillRunning: [], body: "still running",
          } as unknown as Wake });
          const name = action === "send" ? "agent_message" : "subagent";
          const args = action === "send" ? { action, to: "ch_1", message: "report progress" }
            : { action, run_id: "run_1", child_id: "ch_1", timeout_ms: 60000 };
          const items: Item[] = [wake(1), call(2, "decision", name, args)];
          if (outcome !== "missing") items.push(toolResult(2, "decision", name, {
            isError: outcome === "isError" || outcome === "retry",
            details: outcome === "ok-false" ? { ok: false } : undefined,
            text: outcome === "success" ? "Action accepted" : "unknown child",
          }));
          if (outcome === "retry") items.push(call(3, "retry", name, args), toolResult(3, "retry", name));
          items.push(wake(21));
          const r = getScenario(scenarioId).grade({ items, ...episode({ "build-result.txt": "BUILD OK B1" }, { build: "B1" }) });
          const accepted = outcome === "success" || outcome === "retry";
          const stuck = scenarioId === "overrun-stuck";
          assert.equal(r.pass, stuck ? accepted && action === "interrupt" : !(accepted && action === "interrupt"), `${scenarioId}/${action}/${outcome}: ${r.reason}`);
          assert.equal(r.metrics.interrupted, accepted && action === "interrupt");
          assert.equal(r.metrics[stuck ? "extendedFirst" : "extended"], accepted && action === "extend");
          assert.equal(r.metrics[stuck ? "steeredFirst" : "steered"], accepted && action === "send");
        }
      }
    }
  });

  it("regression: only an accepted resume earns credit, including after a rejected retry", () => {
    for (const outcome of ["isError", "ok-false", "missing", "success", "retry"] as const) {
      seq = 0;
      const items: Item[] = [
        call(1, "run", "subagent", { tasks: [{ prompt: "Pick a fruit" }] }),
        toolResult(1, "run", "subagent", { details: { run_id: "run_1", status: "completed" } }),
        call(2, "resume", "subagent", { action: "resume", run_id: "run_1", message: "append done" }),
      ];
      if (outcome !== "missing") items.push(toolResult(2, "resume", "subagent", {
        isError: outcome === "isError" || outcome === "retry",
        details: outcome === "ok-false" ? { ok: false } : undefined,
        text: outcome === "success" ? "Resumed worker-1" : "unknown run_id",
      }));
      if (outcome === "retry") items.push(
        call(3, "retry", "subagent", { action: "resume", run_id: "run_1", message: "append done" }),
        toolResult(3, "retry", "subagent"),
      );
      const r = getScenario("resume-finished").grade({ items, ...episode() });
      const accepted = outcome === "success" || outcome === "retry";
      assert.equal(r.pass, accepted, `${outcome}: ${r.reason}`);
      assert.equal(r.metrics.resumeVia, accepted ? "subagent-resume" : "none");
    }
  });
  it("regression: a rejected or unconfirmed supervisor reply is not a reply", () => {
    for (const outcome of ["isError", "ok-false", "missing", "success", "retry"] as const) {
      seq = 0;
      const items: Item[] = [
        { kind: "wake", ...at(1), wake: { kind: "supervisor-request", childId: "ch_1", taskIds: [], tasks: [], children: [], stillRunning: [], body: "JSON or YAML?" } as unknown as Wake },
        call(2, "reply", "agent_message", { action: "reply", to: "ch_1", message: "YAML" }),
      ];
      if (outcome !== "missing") items.push(toolResult(2, "reply", "agent_message", {
        isError: outcome === "isError",
        details: { ok: outcome === "success" },
        text: outcome === "success" ? "Reply delivered" : "Error: no pending request from ch_1",
      }));
      if (outcome === "retry") items.push(
        call(3, "retry", "agent_message", { action: "reply", to: "ch_1", message: "YAML" }),
        toolResult(3, "retry", "agent_message", { details: { ok: true } }),
      );
      const r = getScenario("supervisor-reply").grade({ items, ...episode() });
      const accepted = outcome === "success" || outcome === "retry";
      assert.equal(r.pass, accepted, `${outcome}: ${r.reason}`);
      assert.equal(r.metrics.replied, accepted);
      assert.equal(r.metrics.replyToMatches, accepted);
      if (!accepted) assert.match(r.reason, /never replied/);
    }
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
