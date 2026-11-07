import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import type { Item } from "../lib/transcript.ts";
import type { Wake } from "../lib/wake-adapter.ts";
import { gradeJob, gradeReady } from "./agent-wait-scenarios.ts";
import { getScenario, SCENARIOS } from "./scenarios.ts";

const T = "0123456789abcdef";
const call = (seq: number, name: string, args: Record<string, unknown>): Item => ({ kind: "assistant", seq, t: seq, text: "", toolCalls: [{ id: `c${seq}`, name, args }] });
const say = (seq: number, text: string): Item => ({ kind: "assistant", seq, t: seq, text, toolCalls: [] });
const result = (seq: number, toolName: string, details: Record<string, unknown>, text = ""): Item => ({ kind: "toolResult", seq, t: seq, toolName, toolCallId: `c${seq - 1}`, details, isError: false, text });
const wake = (seq: number, kind: string, status: string, body: string, taskId: string): Item => ({ kind: "wake", seq, t: seq, wake: { kind, status, body, taskIds: [taskId], tasks: [], children: [], stillRunning: [] } as unknown as Wake });
const started = (seq: number, id: string, timeout: number | null) => result(seq, "monitor", { task_id: id, timeout_ms: timeout }, "Monitor started");

describe("agent-wait-job", () => {
  const viaBash = (): Item[] => [
    call(1, "bash", { command: "node job.cjs" }), result(2, "bash", {}, 'Command "node job.cjs" moved to background (task_id: sh_1).'), say(3, ""),
    wake(4, "task", "completed", `processed 4/5\nRESULT ${T}`, "sh_1"), say(5, `RESULT ${T}`),
  ];
  const viaMonitor = (timeout: number | null): Item[] => [
    call(1, "monitor", { command: "node job.cjs", description: "batch" }), started(2, "mon_1", timeout),
    wake(3, "monitor", "event", `RESULT ${T}`, "mon_1"), say(4, `Done: RESULT ${T}`),
  ];

  it("passes the completion wake and a persistent monitor", () => {
    assert.deepEqual([gradeJob(viaBash(), T).pass, gradeJob(viaBash(), T).metrics.path], [true, "bash"]);
    assert.deepEqual([gradeJob(viaMonitor(null), T).pass, gradeJob(viaMonitor(null), T).metrics.path], [true, "monitor"]);
  });

  it("fails sleep, polling, a timed monitor, a detached launch and a missing report", () => {
    const b = viaBash();
    assert.equal(gradeJob([...b.slice(0, 3), call(3.5, "bash", { command: "sleep 20; tail -1 out" }), ...b.slice(3)], T).pass, false);
    assert.equal(gradeJob([...b.slice(0, 3), call(3.5, "task_output", { task_id: "sh_1" }), ...b.slice(3)], T).pass, false);
    assert.equal(gradeJob(viaMonitor(1_800_000), T).pass, false);
    assert.equal(gradeJob(viaMonitor(1_800_000), T).metrics.timedMonitors, 1);
    const detached = [call(1, "bash", { command: "(node job.cjs > out.txt 2>&1 &)" }), result(2, "bash", {}), call(3, "monitor", { command: "tail -F out.txt" }), started(4, "mon_1", null), wake(5, "monitor", "event", `RESULT ${T}`, "mon_1"), say(6, T)];
    assert.equal(gradeJob(detached, T).pass, false);
    assert.equal(gradeJob(detached, T).metrics.path, "detached");
    assert.equal(gradeJob(b.slice(0, 4), T).pass, false, "no report");
    assert.equal(gradeJob([call(1, "bash", { command: "node job.cjs" }), say(2, `RESULT ${T}`)], T).pass, false, "a report before any evidence is a guess");
  });
});

describe("agent-wait-ready", () => {
  const smoke = (seq: number, ok: boolean): Item[] => [call(seq, "bash", { command: "node smoke.cjs" }), result(seq + 1, "bash", {}, ok ? `SMOKE ok ${T}` : "SMOKE fail: server not ready")];
  const good = (timeout: number | null = null): Item[] => [
    call(1, "monitor", { command: "node server.cjs", description: "dev server" }), started(2, "mon_1", timeout),
    wake(3, "monitor", "event", "listening, ready", "mon_1"), ...smoke(4, true), say(6, `SMOKE ok ${T}`),
  ];

  it("passes a persistent server monitor with the smoke test after its ready event", () => {
    assert.equal(gradeReady(good(), T).pass, true);
  });

  it("passes a server started by pi's bash with a persistent monitor on its ready marker", () => {
    const items = [call(1, "bash", { command: "node server.cjs" }), result(2, "bash", {}, "moved to background (task_id: sh_1)"),
      call(3, "monitor", { command: "while [ ! -f .ready ]; do sleep 1; done; echo READY" }), started(4, "mon_1", null),
      wake(5, "monitor", "event", "READY", "mon_1"), ...smoke(6, true), say(8, T)];
    assert.deepEqual([gradeReady(items, T).pass, gradeReady(items, T).metrics.path], [true, "bash+monitor"]);
    const timed = [...items.slice(0, 3), started(4, "mon_1", 120_000), ...items.slice(4)];
    assert.equal(gradeReady(timed, T).pass, false, "readiness from a timed monitor");
  });

  it("fails a timed server monitor, a smoke test before ready, polling, bash and detached servers", () => {
    assert.equal(gradeReady([...good(), call(7, "task_list", {})], T).pass, true, "a cleanup check after the work is not a poll");
    assert.equal(gradeJob([call(1, "bash", { command: "node job.cjs" }), result(2, "bash", {}, "moved to background (task_id: sh_1)"), wake(3, "task", "completed", `RESULT ${T}`, "sh_1"), say(4, T), call(5, "task_list", {})], T).pass, true);
    assert.equal(gradeReady(good(600_000), T).pass, false);
    assert.equal(gradeReady([...good().slice(0, 2), ...smoke(2.2, false), ...good().slice(2)], T).pass, false);
    assert.equal(gradeReady([...good().slice(0, 2), call(2.5, "task_list", {}), ...good().slice(2)], T).pass, false);
    const viaBash = [call(1, "bash", { command: "node server.cjs" }), result(2, "bash", {}, "moved to background (task_id: sh_1)"), call(3, "bash", { command: "sleep 30; node smoke.cjs" }), result(4, "bash", {}, `SMOKE ok ${T}`), say(5, T)];
    assert.equal(gradeReady(viaBash, T).pass, false);
    assert.equal(gradeReady(viaBash, T).metrics.path, "bash");
    assert.equal(gradeReady(viaBash, T).metrics.readyFrom, "none");
    const detached = [call(1, "bash", { command: "node server.cjs > s.log 2>&1 &" }), result(2, "bash", {}), call(3, "monitor", { command: "tail -F s.log | grep --line-buffered ready" }), started(4, "mon_1", null), wake(5, "monitor", "event", "listening, ready", "mon_1"), ...smoke(6, true), say(8, T)];
    assert.equal(gradeReady(detached, T).pass, false);
    assert.equal(gradeReady(good().slice(0, 5), T).pass, false, "no report");
  });
});

describe("agent-wait fixtures", () => {
  const dirs: string[] = [];
  after(() => { for (const d of dirs) rmSync(d, { recursive: true, force: true }); });
  const view = (id: string) => {
    const cwd = mkdtempSync(join(tmpdir(), "agent-wait-")); const secretDir = mkdtempSync(join(tmpdir(), "agent-wait-secret-"));
    dirs.push(cwd, secretDir);
    return { cwd, secretDir, setup: getScenario(id).setup(cwd, secretDir) };
  };

  it("are registered as opt-in", () => {
    for (const id of ["agent-wait-job", "agent-wait-ready"]) assert.equal(SCENARIOS.find((s) => s.id === id)?.optIn, true);
  });

  it("job prints the token as its last line; server becomes ready and smoke reads it", async () => {
    const j = view("agent-wait-job");
    const out = execFileSync("node", ["job.cjs"], { cwd: j.cwd, env: { ...process.env, ...j.setup.env, JOB_STEP_MS: "10" } }).toString().trim().split("\n");
    assert.equal(out.at(-1), `RESULT ${j.setup.env!.JOB_TOKEN}`);

    const s = view("agent-wait-ready");
    const env = { ...process.env, ...s.setup.env, READY_MS: "200" };
    const early = spawnSync("node", ["smoke.cjs"], { cwd: s.cwd, env });
    assert.deepEqual([early.status, /not ready/.test(early.stdout.toString())], [1, true], "smoke before the server");
    const server = spawn("node", ["server.cjs"], { cwd: s.cwd, env });
    try {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("server not ready")), 5000);
        server.stdout.on("data", (b: Buffer) => { if (b.toString().includes("ready")) { clearTimeout(timer); resolve(); } });
      });
      assert.equal(execFileSync("node", ["smoke.cjs"], { cwd: s.cwd, env }).toString().trim(), `SMOKE ok ${s.setup.env!.SERVER_TOKEN}`);
    } finally { server.kill(); }
  });

  it("keep fixtures free of comments and scenario words", () => {
    for (const [id, files] of [["agent-wait-job", ["job.cjs"]], ["agent-wait-ready", ["server.cjs", "smoke.cjs"]]] as const) {
      const v = view(id);
      for (const f of files) {
        const src = execFileSync("cat", [join(v.cwd, f)]).toString();
        assert.doesNotMatch(src, /\/\/|\/\*/, `${f}: comment`);
        assert.doesNotMatch(src, /monitor|persistent|timeout|poll|sleep/i, `${f}: leaky word`);
      }
    }
  });
});
