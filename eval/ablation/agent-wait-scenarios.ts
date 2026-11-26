/**
 * Opt-in probes where the agent itself has to wait, with no "watch" or
 * "monitor" in the request. The monitor-behavior-* probes all begin with the
 * user asking for a watch; the 2026-11-07 log-monitor session failed at the
 * other kind of decision point: the agent started a job or a server and then
 * slept, polled task_output/ps, detached with `&`, or sized a timeout.
 *
 *   agent-wait-job    a ~30 s job (longer than the 20 s foreground budget):
 *                     its completion wake or a persistent monitor both pass.
 *   agent-wait-ready  a server that never exits and is ready after ~30 s:
 *                     it never sends a completion wake, so readiness has to
 *                     come from a persistent monitor (on the server itself,
 *                     its output or its ready marker) before the smoke test.
 *
 * Lifetimes are graded from the monitor's start result (timeout_ms null =
 * persistent), so a bare start passes where the extension defaults to
 * persistent. Fixtures are comment-free and take their timing from env.
 */
import { randomBytes } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { isDetachedLaunch } from "../replay/wait-choice.ts";
import { assistantTextBetween, callsBetween, cmd, isPoll, type Grade } from "./graders.ts";
import { scenarioText } from "./scenario-text.ts";
import type { Scenario } from "./scenarios.ts";

const jobSource = `const steps = 5;
let n = 0;
const timer = setInterval(() => {
  n++;
  if (n < steps) console.log('processed ' + n + '/' + steps);
  else { console.log('RESULT ' + process.env.JOB_TOKEN); clearInterval(timer); }
}, Number(process.env.JOB_STEP_MS));
`;
const serverSource = `const { writeFileSync } = require('node:fs');
console.log('starting dev server');
const up = setInterval(() => {
  writeFileSync('.ready', process.env.SERVER_TOKEN);
  console.log('listening, ready');
  clearInterval(up);
}, Number(process.env.READY_MS));
setInterval(() => {}, 1000);
`;
const smokeSource = `const { existsSync, readFileSync } = require('node:fs');
if (!existsSync('.ready')) { console.log('SMOKE fail: server not ready'); process.exit(1); }
console.log('SMOKE ok ' + readFileSync('.ready', 'utf8'));
`;

/**
 * Waits and polls inside the waiting window only: after the launch, before the
 * evidence the agent was waiting for (or to the end without it). A task_list
 * after the work is done checks cleanup; it does not wait for anything.
 * Bash waits are any sleep, a wait builtin, a task-output file or a process listing (graders.isPoll).
 */
const bashWaits = (items: Item[], from: number, to: number) => callsBetween(items, from, to).filter((c) => c.name === "bash" && isPoll(c));
const toolPolls = (items: Item[], from: number, to: number) => callsBetween(items, from, to).filter((c) => c.name !== "bash" && isPoll(c));

function monitorLifetimes(items: Item[], runs: (command: string) => boolean = () => true) {
  const results = toolResults(items);
  return callsBetween(items, -1).filter((c) => c.name === "monitor" && runs(cmd(c))).map((c) => {
    const r = results.find((x) => x.toolCallId === c.id);
    const ok = !!r && !r.isError && typeof r.details?.task_id === "string";
    return { call: c, ok, taskId: ok ? String(r!.details!.task_id) : "", persistent: ok && r!.details!.timeout_ms === null };
  });
}

/** Seq of the first wake or tool result that carried `text`, or Infinity. */
function firstEvidence(items: Item[], text: string): number {
  const seqs = [
    ...wakes(items).filter((w) => w.wake.body.includes(text)).map((w) => w.seq),
    ...toolResults(items).filter((r) => r.text.includes(text)).map((r) => r.seq),
  ];
  return seqs.length ? Math.min(...seqs) : Number.POSITIVE_INFINITY;
}

const job: Scenario = {
  id: "agent-wait-job",
  behavior: scenarioText("agent-wait-job.tests"),
  optIn: true, famulusConfig: {}, timeoutMs: 120_000, quietMs: 2000, estCalls: 4,
  setup(cwd, secretDir) {
    const token = randomBytes(8).toString("hex");
    writeFileSync(join(cwd, "job.cjs"), jobSource);
    writeFileSync(join(secretDir, "token"), token);
    return { prompt: scenarioText("agent-wait-job.prompt"), env: { JOB_TOKEN: token, JOB_STEP_MS: "6000" } };
  },
  done(items, ep) { return this.grade(ep).pass === true; },
  grade(ep) { return gradeJob(ep.items, tokenOf(ep.secretDir)); },
};

const ready: Scenario = {
  id: "agent-wait-ready",
  behavior: scenarioText("agent-wait-ready.tests"),
  optIn: true, famulusConfig: {}, timeoutMs: 120_000, quietMs: 2000, estCalls: 5,
  setup(cwd, secretDir) {
    const token = randomBytes(8).toString("hex");
    writeFileSync(join(cwd, "server.cjs"), serverSource);
    writeFileSync(join(cwd, "smoke.cjs"), smokeSource);
    writeFileSync(join(secretDir, "token"), token);
    return { prompt: scenarioText("agent-wait-ready.prompt"), env: { SERVER_TOKEN: token, READY_MS: "30000" } };
  },
  done(items, ep) { return this.grade(ep).pass === true; },
  grade(ep) { return gradeReady(ep.items, tokenOf(ep.secretDir)); },
};

function tokenOf(secretDir: string): string {
  try { return readFileSync(join(secretDir, "token"), "utf8"); } catch { return ""; }
}

const runsJob = (c: string) => /\bnode\s+\S*job\.cjs\b/.test(c);
const runsServer = (c: string) => /\bnode\s+\S*server\.cjs\b/.test(c);

export function gradeJob(items: Item[], token: string): Grade {
  const calls = callsBetween(items, -1);
  const launch = calls.find((c) => (c.name === "bash" || c.name === "monitor") && runsJob(cmd(c)));
  const from = launch?.seq ?? -1;
  const monitors = monitorLifetimes(items, runsJob);
  const detached = calls.some((c) => c.name === "bash" && runsJob(cmd(c)) && isDetachedLaunch(cmd(c)));
  const evidence = token ? firstEvidence(items, `RESULT ${token}`) : Number.POSITIVE_INFINITY;
  const reported = Number.isFinite(evidence) && assistantTextBetween(items, evidence).includes(token);
  const sleeps = launch ? bashWaits(items, from, evidence).length : 0;
  const polls = launch ? toolPolls(items, from, evidence).length : 0;
  const timedMonitors = monitors.filter((m) => m.ok && !m.persistent).length;
  const path = !launch ? "none" : launch.name === "monitor" ? "monitor" : detached ? "detached" : "bash";
  const metrics = { path, reported, sleeps, polls, detached, monitorStarts: monitors.filter((m) => m.ok).length, timedMonitors };
  if (!token) return { pass: null, reason: "fixture token missing", metrics };
  const pass = !!launch && reported && !detached && sleeps === 0 && polls === 0 && timedMonitors === 0;
  return { pass, reason: pass ? `waited through ${path}` : "waited badly or did not report", metrics };
}

export function gradeReady(items: Item[], token: string): Grade {
  const calls = callsBetween(items, -1);
  const monitors = monitorLifetimes(items);
  const serverMonitor = monitors.find((m) => m.ok && runsServer(cmd(m.call)));
  const bashServer = calls.find((c) => c.name === "bash" && runsServer(cmd(c)));
  const detached = calls.some((c) => c.name === "bash" && runsServer(cmd(c)) && isDetachedLaunch(cmd(c)));
  const results = toolResults(items);
  const smokes = calls.filter((c) => c.name === "bash" && /\bnode\s+\S*smoke\.cjs\b/.test(cmd(c)));
  const smokeOk = smokes.find((c) => results.some((r) => r.toolCallId === c.id && r.text.includes(`SMOKE ok ${token}`)));
  const earlySmokes = smokes.filter((c) => results.some((r) => r.toolCallId === c.id && r.text.includes("not ready"))).length;
  // Readiness learned from a monitor: the first event wake of any started monitor before the good smoke run.
  const readyFrom = monitors.filter((m) => m.ok).map((m) => ({ m, w: wakes(items).find((w) => w.wake.kind === "monitor" && w.wake.status === "event" && w.wake.taskIds.includes(m.taskId)) }))
    .filter((x) => !!x.w && (!smokeOk || x.w.seq < smokeOk.seq)).sort((a, b) => a.w!.seq - b.w!.seq)[0];
  const reported = !!smokeOk && assistantTextBetween(items, smokeOk.seq).includes(token);
  const from = Math.min(serverMonitor?.call.seq ?? Infinity, bashServer?.seq ?? Infinity);
  const until = smokeOk?.seq ?? Number.POSITIVE_INFINITY;
  const sleeps = Number.isFinite(from) ? bashWaits(items, from, until).length : 0;
  const polls = Number.isFinite(from) ? toolPolls(items, from, until).length : 0;
  const path = detached ? "detached" : serverMonitor ? "server-monitor" : bashServer ? (readyFrom ? "bash+monitor" : "bash") : "none";
  const metrics = { path, reported, readyFrom: readyFrom ? (readyFrom.m.persistent ? "persistent-monitor" : "timed-monitor") : "none", smokeAfterReady: !!smokeOk && !!readyFrom, earlySmokes, sleeps, polls, detached, monitorStarts: monitors.filter((m) => m.ok).length };
  if (!token) return { pass: null, reason: "fixture token missing", metrics };
  const pass = Number.isFinite(from) && !detached && !!readyFrom && readyFrom.m.persistent && reported && earlySmokes === 0 && sleeps === 0 && polls === 0;
  return { pass, reason: pass ? `readiness from a persistent monitor (${path})` : "waited badly or did not report", metrics };
}

export const agentWaitScenarios: Scenario[] = [job, ready];
