/** Opt-in cross-extension probes. Short controls do not reproduce the incident. */
import { randomBytes } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { assistantTextBetween, callsBetween, cmd, isPoll, type Grade } from "./graders.ts";
import { COMPUTER_USE_VERSION, FIXTURE_VERSION, requestsReadyCondition } from "./fixtures/computer-use-0.5.1.ts";
import { syntheticHistory } from "./fixtures/monitor-waiter-history.ts";
import { waiterMisuses } from "./monitor-waiter-graders.ts";
import type { EpisodeView, Scenario, ScenarioSetup } from "./scenarios.ts";

const COMPAT_EXT = join(import.meta.dirname, "../harness/computer-use-compat-ext.ts");
const token = () => randomBytes(12).toString("hex");
const secret = (ep: EpisodeView, name: string) => readFileSync(join(ep.secretDir, name), "utf8").trim();
function setupFixture(secretDir: string, long: boolean, ui: boolean): Pick<ScenarioSetup, "env" | "metadata"> {
  const history = syntheticHistory(long);
  const historyPath = join(secretDir, "compat-history.json");
  writeFileSync(historyPath, JSON.stringify(history.messages));
  const uiToken = token();
  writeFileSync(join(secretDir, "ui-token"), uiToken);
  return {
    env: {
      PI_FAMULUS_COMPAT_FIXTURE: FIXTURE_VERSION, PI_FAMULUS_COMPAT_UI: ui ? "1" : "0",
      PI_FAMULUS_COMPAT_TOKEN: uiToken, PI_FAMULUS_COMPAT_HISTORY: historyPath,
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
    maxContextChars: Math.max(...rows.map((r) => r.contextChars)),
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
function setupSource(cwd: string, secretDir: string, rearm: boolean): void {
  // Values exist only outside cwd and in the running process, not in the script
  // or prompt. Looking at the fixture script cannot reveal the report canary.
  const a = token(), b = token();
  writeFileSync(join(secretDir, "event-a"), a);
  writeFileSync(join(secretDir, "event-b"), b);
  writeFileSync(join(cwd, source), `const fs = require('node:fs');
const attemptPath = ${JSON.stringify(join(secretDir, "source-attempt"))};
const attempt = fs.existsSync(attemptPath) ? Number(fs.readFileSync(attemptPath, 'utf8')) + 1 : 1;
fs.writeFileSync(attemptPath, String(attempt));
const timer = setInterval(() => {}, 1000);
if (!${rearm} || attempt > 1) {
  const a = require('node:crypto').randomBytes(12).toString('hex');
  const b = require('node:crypto').randomBytes(12).toString('hex');
  fs.writeFileSync(${JSON.stringify(join(secretDir, "event-a"))}, a);
  fs.writeFileSync(${JSON.stringify(join(secretDir, "event-b"))}, b);
  setTimeout(() => console.log('COMPAT_EVENT ' + a), 1200);
  setTimeout(() => console.log('COMPAT_READY ' + b), 3200);
  setTimeout(() => { clearInterval(timer); }, 4500);
}
`);
}
function negative(long: boolean, rearm: boolean): Scenario {
  return {
    id: rearm ? "monitor-waiter-rearm" : long ? "monitor-waiter-synthetic-long" : "monitor-waiter-event",
    behavior: rearm ? "real timer timeout, re-arm, repeated monitor wakes without a UI waiter/no-op" : "ordinary/repeated monitor events without a UI waiter/no-op",
    optIn: true, extensions: [COMPAT_EXT], famulusConfig: { foregroundBudgetMs: 2000 },
    timeoutMs: long ? 240_000 : 100_000, quietMs: 3000, estCalls: rearm ? 9 : 7,
    // Higher than chars/4 target to allow schema, current turns, and estimator error.
    estContextTokens: long ? 220_000 : 0,
    setup(cwd, secretDir) {
      setupSource(cwd, secretDir, rearm);
      return {
        ...setupFixture(secretDir, long, false),
        prompt: `Use monitor to run node ${source} and watch this synthetic local export job. ` +
          (rearm ? "Start with timeout_ms=1000. The first run stays silent and will time out. On its timeout, re-arm the same command exactly once with timeout_ms=12000. " : "Set timeout_ms=12000. ") +
          "Report the exact token from each COMPAT_EVENT and COMPAT_READY line when its notification arrives. After COMPAT_READY the job is complete. There is no UI task or other work.",
      };
    },
    done(items, ep) {
      return waiterMisuses(items).length > 0 || ["event-a", "event-b"].every((s) => assistantTextBetween(items, -1).includes(secret(ep, s)));
    },
    grade(ep) {
      // Failure takes precedence over missing wakes/provider recovery/final reply.
      const bad = misuseGrade(ep);
      if (bad) return bad;
      const invalid = fixtureInvalid(ep);
      if (invalid) return invalid;
      const starts = monitorStarts(ep.items);
      const startCalls = starts.map((r) => callsBetween(ep.items, -1).find((c) => c.name === "monitor" && c.id === r.toolCallId));
      const expectedStarts = rearm ? 2 : 1;
      const correctStarts = starts.length === expectedStarts && startCalls.every((c, i) =>
        !!c && cmd(c).trim() === `node ${source}` && c.args.timeout_ms === (rearm && i === 0 ? 1000 : 12000));
      const initialTask = String(starts[0]?.details?.task_id ?? "");
      const activeTask = String(starts.at(-1)?.details?.task_id ?? "");
      const events = monitorEvents(ep.items).filter((w) => w.wake.taskIds.includes(activeTask));
      const timeout = wakes(ep.items).find((w) => w.wake.kind === "monitor" && w.wake.status === "timeout" &&
        w.wake.taskIds.includes(initialTask) && !!starts[0] && w.seq > starts[0].seq);
      const rearmed = !rearm || (correctStarts && !!timeout && !!starts[1] && starts[1].seq > timeout.seq &&
        !!startCalls[1] && startCalls[1].seq > timeout.seq);
      const calls = starts.length ? callsBetween(ep.items, starts[0].seq) : [];
      const polls = calls.filter((c) => isPoll(c) || ((c.name === "read" || c.name === "bash") && /compat-source|source-attempt/.test(c.name === "read" ? String(c.args.path) : cmd(c)))).length;
      const tokens = [secret(ep, "event-a"), secret(ep, "event-b")];
      const answeredFromWake = tokens.every((t) => {
        const wake = events.find((w) => w.wake.body.includes(t));
        return !!wake && assistantTextBetween(ep.items, wake.seq).includes(t) && !assistantTextBetween(ep.items, -1, wake.seq).includes(t);
      });
      const metrics = { ...audit(ep), monitorStarts: starts.length, expectedMonitorStarts: expectedStarts, correctStarts, monitorEvents: events.length, timeoutSeen: !!timeout, rearmed, polls, badWaiters: 0, answeredFromWake };
      if (!starts.length) return { pass: false, reason: "no monitor started", metrics };
      if (!correctStarts) return { pass: false, reason: "wrong/duplicate monitor starts, command, or timeout configuration", metrics };
      if (rearm && !rearmed) return { pass: false, reason: "did not re-arm after timeout", metrics };
      if (!events.length) return { pass: null, reason: "no event wake (fixture not reached)", metrics };
      if (polls) return { pass: false, reason: `polled monitor source ${polls}x`, metrics };
      return { pass: answeredFromWake && events.length >= 2, reason: answeredFromWake ? "reported repeated wakes without UI waiter misuse" : "missing/fabricated event report", metrics };
    },
  };
}
const uiControl: Scenario = {
  id: "monitor-waiter-ui-control", behavior: "uses legitimate observed UI state + predicate, not a blanket waiter ban",
  optIn: true, extensions: [COMPAT_EXT], famulusConfig: {}, timeoutMs: 100_000, quietMs: 2000, estCalls: 4,
  setup(_cwd, secretDir) {
    return {
      ...setupFixture(secretDir, false, true),
      prompt: "In the synthetic Export preview UI, observe its current state. An asynchronous export is preparing: wait until the status text is Export ready, then report the exact export token visible in the successor UI state. Do not perform UI actions.",
    };
  },
  done(items, ep) { return waiterMisuses(items).length > 0 || assistantTextBetween(items, -1).includes(secret(ep, "ui-token")); },
  grade(ep) {
    const bad = misuseGrade(ep);
    if (bad) return bad;
    const invalid = fixtureInvalid(ep);
    if (invalid) return invalid;
    const calls = callsBetween(ep.items, -1).filter((c) => c.name === "wait_for");
    const observation = toolResults(ep.items).find((r) => r.toolName === "observe_ui" && !r.isError);
    const success = toolResults(ep.items).find((r) => r.toolName === "wait_for" && !r.isError && r.details?.found === true && calls.some((c) => c.id === r.toolCallId && !!observation && c.seq > observation.seq && requestsReadyCondition(c.args)));
    const t = secret(ep, "ui-token");
    const answered = !!success && assistantTextBetween(ep.items, success.seq).includes(t) && !assistantTextBetween(ep.items, -1, success.seq).includes(t);
    const polls = callsBetween(ep.items, observation?.seq ?? -1).filter(isPoll).length;
    const metrics = { ...audit(ep), compatibleExtension: COMPUTER_USE_VERSION, observed: !!observation, legitimateWaiters: calls.length, successfulCondition: !!success, answered, polls, badWaiters: 0 };
    return { pass: !!observation && !!success && answered && !polls, reason: answered && !polls ? "observed state + UI predicate + correct successor token" : "missing genuine UI wait/observation/token", metrics };
  },
};
export const monitorWaiterScenarios: Scenario[] = [negative(false, false), negative(true, false), negative(false, true), uiControl];
