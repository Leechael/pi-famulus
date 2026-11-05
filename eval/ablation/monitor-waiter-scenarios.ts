/** Opt-in cross-extension probes. Short controls do not reproduce the incident. */
import { scenarioText } from "./scenario-text.ts";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { assistantTextBetween, callsBetween, cmd, isPoll, type Grade } from "./graders.ts";
import { COMPUTER_USE_VERSION, FIXTURE_VERSION, requestsReadyCondition } from "./fixtures/computer-use-0.5.1.ts";
import { syntheticHistory } from "./fixtures/monitor-waiter-history.ts";
import { waiterMisuses } from "./monitor-waiter-graders.ts";
import type { EpisodeView, Scenario, ScenarioSetup } from "./scenarios.ts";

const COMPAT_EXT = join(import.meta.dirname, "../harness/computer-use-compat-ext.ts");
function setupFixture(secretDir: string, long: boolean, ui: boolean): Pick<ScenarioSetup, "env" | "metadata"> {
  const history = syntheticHistory(long);
  const historyPath = join(secretDir, "compat-history.json");
  writeFileSync(historyPath, JSON.stringify(history.messages));
  return {
    env: {
      PI_FAMULUS_COMPAT_FIXTURE: FIXTURE_VERSION, PI_FAMULUS_COMPAT_UI: ui ? "1" : "0",
      PI_FAMULUS_COMPAT_HISTORY: historyPath,
      PI_FAMULUS_COMPAT_AUDIT: join(secretDir, "compat-context.jsonl"),
    },
    metadata: { ...history.metadata, uiExecutor: "in-memory stub; no native/UI/network actions", evalDevSdkVersion: "1.0.0" },
  };
}
function audit(ep: EpisodeView): Grade["metrics"] {
  const path = join(ep.secretDir, "compat-context.jsonl");
  if (!existsSync(path)) return { fixtureRequests: 0 };
  const rows = readFileSync(path, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
  return rows.length ? {
    ...rows[0], fixtureRequests: rows.length,
    maxContextCharsEstimate: Math.max(...rows.map((r) => r.contextCharsEstimate)),
    maxContextEstimatedTokens: Math.max(...rows.map((r) => r.contextEstimatedTokens)),
  } : { fixtureRequests: 0 };
}
function fixtureInvalid(ep: EpisodeView): Grade | undefined {
  const metrics = audit(ep);
  if (!metrics.fixtureRequests || !metrics.waitForLoaded || !metrics.observeUiLoaded) {
    return { pass: null, reason: "compatible tools/context fixture not loaded", metrics };
  }
}
function misuseGrade(ep: EpisodeView): Grade | undefined {
  const bad = waiterMisuses(ep.items);
  if (!bad.length) return;
  return { pass: false, reason: `bad wait_for call (${bad[0].reasons.join("; ")})`, metrics: { ...audit(ep), badWaiters: bad.length, waiterCalls: callsBetween(ep.items, -1).filter((c) => c.name === "wait_for").length } };
}
const monitorStarts = (items: Item[]) => toolResults(items).filter((r) => r.toolName === "monitor" && !r.isError && r.details?.task_id);
const monitorEvents = (items: Item[]) => wakes(items).filter((w) => w.wake.kind === "monitor" && w.wake.status === "event");
const source = "compat-source.cjs";
function hasWakeReports(items: Item[]): boolean {
  const events = monitorEvents(items);
  return events.length > 0 && events.every((w, i) => {
    const next = events[i + 1]?.seq ?? Number.POSITIVE_INFINITY;
    const tokens = [...w.wake.body.matchAll(/COMPAT_(?:EVENT|READY)\s+([a-f0-9]{24})/g)].map((m) => m[1]);
    return tokens.length > 0 && tokens.every((t) => assistantTextBetween(items, w.seq, next).includes(t));
  }) && ["COMPAT_EVENT", "COMPAT_READY"].every((kind) => events.some((w) => w.wake.body.includes(kind)));
}
function hasUiWakeReport(items: Item[]): boolean {
  return toolResults(items).some((r) => {
    if (r.toolName !== "wait_for" || r.isError || r.details?.found !== true) return false;
    const rendered = r.details?.renderedOutline;
    const expected = typeof rendered === "string" ? rendered.match(/@e3 AXStaticText "([^"]+)"/)?.[1] : undefined;
    return !!expected && assistantTextBetween(items, r.seq).includes(expected);
  });
}
function sourceContents(rearm: boolean): string {
  return `const fs = require('node:fs');
const attemptPath = require('node:path').join(process.cwd(), '.compat-source-attempt');
const attempt = fs.existsSync(attemptPath) ? Number(fs.readFileSync(attemptPath, 'utf8')) + 1 : 1;
fs.writeFileSync(attemptPath, String(attempt));
const timer = setInterval(() => {}, 1000);
if (!${rearm} || attempt > 1) {
  const a = require('node:crypto').randomBytes(12).toString('hex');
  const b = require('node:crypto').randomBytes(12).toString('hex');
  setTimeout(() => console.log('COMPAT_EVENT ' + a), 1200);
  setTimeout(() => console.log('COMPAT_READY ' + b), 3200);
  setTimeout(() => { clearInterval(timer); }, 4500);
}
`;
}
function sourceWasModified(ep: EpisodeView, rearm: boolean): boolean {
  const path = join(ep.cwd, source);
  if (existsSync(path) && createHash("sha256").update(readFileSync(path)).digest("hex") !== createHash("sha256").update(sourceContents(rearm)).digest("hex")) return true;
  return callsBetween(ep.items, -1).some((call) => {
    if (["read", "write", "edit"].includes(call.name)) return basename(String(call.args.path ?? "")) === source;
    // Any model-issued shell access to the fixture source is outside the task
    // contract; it can read or rewrite the producer even if restored afterward.
    return call.name === "bash" && cmd(call).includes(source);
  });
}
function setupSource(cwd: string, _secretDir: string, rearm: boolean): void {
  writeFileSync(join(cwd, source), sourceContents(rearm));
}
function negative(long: boolean, rearm: boolean): Scenario {
  const id = rearm ? "monitor-waiter-rearm" : long ? "monitor-waiter-synthetic-long" : "monitor-waiter-event";
  return {
    id,
    behavior: scenarioText(`${id}.tests`),
    optIn: true, extensions: [COMPAT_EXT], famulusConfig: { foregroundBudgetMs: 2000 },
    timeoutMs: long ? 240_000 : 100_000, quietMs: 3000, estCalls: rearm ? 9 : 7,
    // Higher than chars/4 target to allow schema, current turns, and estimator error.
    estContextTokens: long ? 220_000 : 0,
    setup(cwd, secretDir) {
      setupSource(cwd, secretDir, rearm);
      return {
        ...setupFixture(secretDir, long, false),
        prompt: scenarioText(`${id}.prompt`, { source }),
      };
    },
    done(items) { return waiterMisuses(items).length > 0 || hasWakeReports(items); },
    grade(ep) {
      // Failure takes precedence over missing wakes/provider recovery/final reply.
      const bad = misuseGrade(ep);
      if (bad) return bad;
      const invalid = fixtureInvalid(ep);
      if (invalid) return invalid;
      if (sourceWasModified(ep, rearm)) return { pass: false, reason: "fixture source was modified or accessed through a model shell/tool", metrics: { ...audit(ep), fixtureSourceIntact: false, badWaiters: 0 } };
      const starts = monitorStarts(ep.items);
      const startCalls = starts.map((r) => callsBetween(ep.items, -1).find((c) => c.name === "monitor" && c.id === r.toolCallId));
      const expectedStarts = rearm ? 2 : 1;
      const correctStarts = starts.length === expectedStarts && startCalls.every((c, i) =>
        !!c && cmd(c).trim() === `node ${source}` && c.args.timeout_ms === (rearm && i === 0 ? 1000 : 12000));
      const initialTask = String(starts[0]?.details?.task_id ?? "");
      const activeTask = String(starts.at(-1)?.details?.task_id ?? "");
      const events = monitorEvents(ep.items).filter((w) => w.wake.taskIds.includes(activeTask));
      const timeout = wakes(ep.items).find((w) => (w.wake.kind === "monitor" || w.wake.kind === "monitor-terminal") && w.wake.status === "timeout" &&
        w.wake.taskIds.includes(initialTask) && !!starts[0] && w.seq > starts[0].seq);
      const rearmed = !rearm || (correctStarts && !!timeout && !!starts[1] && starts[1].seq > timeout.seq &&
        !!startCalls[1] && startCalls[1].seq > timeout.seq);
      const calls = starts.length ? callsBetween(ep.items, starts[0].seq) : [];
      const polls = calls.filter((c) => isPoll(c) || ((c.name === "read" || c.name === "bash") && /compat-source|source-attempt/.test(c.name === "read" ? String(c.args.path) : cmd(c)))).length;
      const deliveries = events.map((w, i) => ({
        wake: w, next: events[i + 1]?.seq ?? Number.POSITIVE_INFINITY,
        tokens: [...w.wake.body.matchAll(/COMPAT_(?:EVENT|READY)\s+([a-f0-9]{24})/g)].map((m) => m[1]),
      }));
      const answeredFromWake = deliveries.length > 0 && deliveries.every(({ wake, next, tokens }) =>
        tokens.length > 0 && tokens.every((t) => assistantTextBetween(ep.items, wake.seq, next).includes(t)));
      const allKindsDelivered = ["COMPAT_EVENT", "COMPAT_READY"].every((kind) => events.some((w) => w.wake.body.includes(kind)));
      const metrics = { ...audit(ep), fixtureSourceIntact: true, monitorStarts: starts.length, expectedMonitorStarts: expectedStarts, correctStarts, monitorEvents: events.length, timeoutSeen: !!timeout, rearmed, polls, badWaiters: 0, answeredFromWake };
      if (!starts.length) return { pass: false, reason: "no monitor started", metrics };
      if (!correctStarts) return { pass: false, reason: "wrong/duplicate monitor starts, command, or timeout configuration", metrics };
      if (rearm && !rearmed) return { pass: false, reason: "did not re-arm after timeout", metrics };
      if (!events.length) return { pass: null, reason: "no event wake (fixture not reached)", metrics };
      if (polls) return { pass: false, reason: `polled monitor source ${polls}x`, metrics };
      return { pass: answeredFromWake && allKindsDelivered, reason: answeredFromWake && allKindsDelivered ? "reported delivered tokens in bounded monitor wake(s) without UI waiter misuse" : "missing/fabricated event report", metrics };
    },
  };
}
const uiControl: Scenario = {
  id: "monitor-waiter-ui-control", behavior: scenarioText("monitor-waiter-ui-control.tests"),
  optIn: true, extensions: [COMPAT_EXT], famulusConfig: {}, timeoutMs: 100_000, quietMs: 2000, estCalls: 4,
  setup(_cwd, secretDir) {
    return {
      ...setupFixture(secretDir, false, true),
      prompt: scenarioText("monitor-waiter-ui-control.prompt"),
    };
  },
  done(items) { return waiterMisuses(items).length > 0 || hasUiWakeReport(items); },
  grade(ep) {
    const bad = misuseGrade(ep);
    if (bad) return bad;
    const invalid = fixtureInvalid(ep);
    if (invalid) return invalid;
    const calls = callsBetween(ep.items, -1).filter((c) => c.name === "wait_for");
    const observation = toolResults(ep.items).find((r) => r.toolName === "observe_ui" && !r.isError);
    const success = toolResults(ep.items).find((r) => r.toolName === "wait_for" && !r.isError && r.details?.found === true && calls.some((c) => c.id === r.toolCallId && !!observation && c.seq > observation.seq && requestsReadyCondition(c.args)));
    const rendered = success?.details?.renderedOutline;
    const t = typeof rendered === "string" ? rendered.match(/@e3 AXStaticText "([^"]+)"/)?.[1] : undefined;
    const answered = !!success && !!t && assistantTextBetween(ep.items, success.seq).includes(t) && !assistantTextBetween(ep.items, -1, success.seq).includes(t);
    const polls = callsBetween(ep.items, observation?.seq ?? -1).filter(isPoll).length;
    const metrics = { ...audit(ep), compatibleExtension: COMPUTER_USE_VERSION, observed: !!observation, legitimateWaiters: calls.length, successfulCondition: !!success, answered, polls, badWaiters: 0 };
    return { pass: !!observation && !!success && answered && !polls, reason: answered && !polls ? "observed state + UI predicate + correct successor token" : "missing genuine UI wait/observation/token", metrics };
  },
};
type BehaviorProbe = "long" | "hour" | "alternative" | "noise" | "cleanup" | "natural" | "renew";

// The producer emits distinct, low-frequency routine lines before a terminal
// event. Keeping it alive lets the cleanup probe distinguish stopping it from
// simply observing an already-exited process. In slow mode the alert lands at
// 24s, after the 8s timeout and 10s terminal batch. Keep the fixture itself
// comment-free: models cat it, and comments would leak the scenario's intent.
const behaviorSource = `let n = 0;
const timer = setInterval(() => {
  n++;
  console.log(n < 3 ? 'ROUTINE ' + n : 'ALERT fixture-ready');
  if (n === 3) clearInterval(timer);
}, Number(process.env.SRC_INTERVAL_MS) * (process.argv.includes('slow') ? 10 : 1));
setInterval(() => {}, 1000);
`;
const BEHAVIOR_TESTS: Record<BehaviorProbe, string> = {
  long: "open-ended observation (\"no planned end\") selects a persistent monitor and reports its alert",
  hour: "an explicit one-hour observation selects timeout_ms 3600000, not persistence",
  alternative: "an already-running alternative monitor delivers the alert after the other monitor's timeout, without another start",
  noise: "a persistent observation suppresses routine-line wakes and reports the alert",
  cleanup: "after the alert, an accepted task_stop targets the active monitor",
  natural: "", renew: "",
};
function behaviorProbe(kind: BehaviorProbe): Scenario {
  const id = `monitor-behavior-${kind}`;
  const alternative = kind === "alternative";
  const cleanup = kind === "cleanup";
  const natural = kind === "natural";
  const renew = kind === "renew";
  const slow = alternative || renew;
  const prompt = alternative
    ? "We need two independent observations of the same stream, `node behavior-source.cjs slow`: start a temporary diagnostic observation lasting eight seconds and, before it expires, a separate ongoing observation. Continue watching for the ALERT and report it."
    : natural ? scenarioText("monitor-behavior-natural.prompt")
    : renew ? scenarioText("monitor-behavior-renew.prompt")
    : `Watch the stream from \`node behavior-source.cjs\`. ${kind === "hour" ? "The observation should last exactly one hour, then end." : "This is ongoing observation with no planned end."} ${kind === "noise" ? "ROUTINE lines are unimportant; notify me only about ALERT lines." : "Report the ALERT line when it arrives."}${cleanup ? " After the alert, the observation is no longer needed; stop it and confirm completion." : ""}`;
  return {
    id, behavior: natural || renew ? scenarioText(`${id}.tests`) : BEHAVIOR_TESTS[kind], optIn: true, famulusConfig: {}, timeoutMs: 120_000, quietMs: 2000, estCalls: 5,
    // renew: the trial's batched timeout notice (≤10 s) lands after its task has left `ls`.
    ...(renew ? { idleGiveUpMs: 20_000 } : {}),
    setup(cwd) {
      writeFileSync(join(cwd, "behavior-source.cjs"), behaviorSource);
      // Timing lives in env so reading the source does not reveal how soon ALERT comes.
      return { prompt, env: { SRC_INTERVAL_MS: "800" } };
    },
    done(_items, ep) { return this.grade(ep).pass === true; },
    grade({ items }) {
      const calls = callsBetween(items, -1);
      const results = toolResults(items);
      const starts = calls.filter((c) => c.name === "monitor");
      const accepted = starts.filter((c) => results.some((r) => r.toolCallId === c.id && !r.isError && r.details?.ok !== false && typeof r.details?.task_id === "string"));
      const start = accepted[alternative || renew ? 1 : 0];
      const started = results.find((r) => r.toolCallId === start?.id);
      const taskId = String(started?.details?.task_id ?? "");
      const events = wakes(items).filter((w) => w.wake.kind === "monitor" && w.wake.status === "event" && w.wake.taskIds.includes(taskId));
      const alert = events.find((w) => w.wake.body.includes("ALERT fixture-ready"));
      const reported = !!alert && assistantTextBetween(items, alert.seq).includes("ALERT fixture-ready");
      const firstResult = results.find((r) => r.toolCallId === accepted[0]?.id);
      const firstId = String(firstResult?.details?.task_id ?? "");
      const timeout = wakes(items).find((w) => (w.wake.kind === "monitor" || w.wake.kind === "monitor-terminal") && w.wake.status === "timeout" && w.wake.taskIds.includes(firstId) && w.seq > ((renew ? firstResult : started)?.seq ?? Infinity));
      const activeAtTimeout = !!timeout && !!started && started.seq < timeout.seq && !!taskId && taskId !== firstId && !wakes(items).some((w) => w.seq < timeout.seq && w.seq > started.seq && w.wake.taskIds.includes(taskId) && (w.wake.kind === "monitor-terminal" || (w.wake.kind === "monitor" && w.wake.status !== "event"))) && !calls.some((c) => c.name === "task_stop" && c.args.task_id === taskId && c.seq < timeout.seq && results.some((r) => r.toolCallId === c.id && !r.isError && r.details?.ok !== false));
      const polls = (starts[0] ? callsBetween(items, starts[0].seq) : []).filter(isPoll).length;
      const routineWakes = events.filter((w) => /ROUTINE/.test(w.wake.body)).length;
      const stopped = cleanup && !!alert && calls.some((c) => c.name === "task_stop" && c.seq > alert.seq && c.args.task_id === taskId && results.some((r) => r.toolCallId === c.id && !r.isError && r.details?.ok !== false));
      const sourceCommand = (command: string) => /\bnode\s+\S*behavior-source\.cjs\b/.test(command) && (!slow || /\bslow\b/.test(command));
      const correctCommand = !!start && sourceCommand(cmd(start)) && (!(alternative || renew) || sourceCommand(cmd(accepted[0])));
      const correctDuration = !!start && (kind === "hour" ? start.args.timeout_ms === 3_600_000 && start.args.persistent !== true : start.args.persistent === true) && (!alternative || (accepted[0]?.args.timeout_ms === 8000 && accepted[0]?.args.persistent !== true));
      const trialMs = accepted[0]?.args.timeout_ms;
      // renew: the trial must be a 5 s timed start; the continuation after its expiry must be persistent, not another timeout.
      const trialOk = !renew || (typeof trialMs === "number" && trialMs >= 1000 && trialMs <= 15_000 && accepted[0]?.args.persistent !== true);
      const continuedAfterTimeout = !renew || (!!timeout && !!start && !!firstResult && typeof trialMs === "number" && start.t >= firstResult.t + trialMs);
      const continuation = !start ? "none" : start.args.persistent === true ? "persistent" : typeof start.args.timeout_ms === "number" ? `timeout:${start.args.timeout_ms}` : "default-timeout";
      const alertAfterTimeout = !!alert && !!timeout && alert.seq > timeout.seq;
      const metrics = { monitorAttempts: starts.length, monitorStarts: accepted.length, correctCommand, correctDuration, continuation, trialOk, continuedAfterTimeout, eventWakes: events.length, routineWakes, alertSeen: !!alert, reported, polls, stopped: !!stopped, timeoutSeen: !!timeout, activeAtTimeout, alertAfterTimeout };
      const duplicateAfterTimeout = !!timeout && starts.some((c) => c.seq > timeout.seq);
      // The decision point (continuation after a real expiry) was not reproduced: INVALID, not FAIL.
      if (renew && accepted.length >= 1 && !trialOk) return { pass: null, reason: "trial not started as a short timed watch", metrics };
      if (renew && start && firstResult && typeof trialMs === "number" && start.t < firstResult.t + trialMs) return { pass: null, reason: "continuation preceded trial expiry", metrics };
      if (renew && accepted.length >= 1 && !timeout) return { pass: null, reason: "trial monitor timeout not observed", metrics };
      if (alternative && !timeout && !duplicateAfterTimeout) return { pass: null, reason: "temporary monitor timeout with an already-running alternative not observed", metrics };
      const pass = starts.length === (alternative || renew ? 2 : 1) && accepted.length === (alternative || renew ? 2 : 1) && trialOk && continuedAfterTimeout && correctCommand && correctDuration && !!alert && reported && !polls && (kind !== "noise" || routineWakes === 0) && (!cleanup || stopped) && (!alternative || (activeAtTimeout && alertAfterTimeout && !duplicateAfterTimeout));
      return { pass, reason: pass ? "behavior and evidence matched" : "behavior or evidence missing", metrics };
    },
  };
}
export const monitorWaiterScenarios: Scenario[] = [negative(false, false), negative(true, false), negative(false, true), uiControl, ...(["long", "hour", "alternative", "noise", "cleanup", "natural", "renew"] as const).map(behaviorProbe)];
