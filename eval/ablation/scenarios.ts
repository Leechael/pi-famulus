/**
 * Real-model probe scenarios. Each is tiny (a few model calls), runs in a
 * temp cwd with its own PI_FAMULUS_HOME, and probes ONE behavior. Canary values are
 * generated at run time by the fixture scripts (written to a secret dir
 * outside the cwd), so the model can only learn them from the wake.
 */
import { type ChildProcess, spawn } from "node:child_process";
import { chmodSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { monitorWaiterScenarios } from "./monitor-waiter-scenarios.ts";
import {
  ackAndStop,
  assistantTextBetween,
  blockedSleeps,
  callsBetween,
  cmd,
  firstWake,
  firstWriteOf,
  type Grade,
  isPoll,
  stated,
  wrongActions,
} from "./graders.ts";
import { scenarioText } from "./scenario-text.ts";

export interface EpisodeView {
  items: Item[];
  cwd: string;
  secretDir: string;
  /** When the episode stopped, on the items' clock (ms since pi started). */
  endedAt?: number;
}

export interface ScenarioSetup {
  prompt: string;
  /** Scenario-local fixture configuration, not inherited by old probes. */
  env?: Record<string, string>;
  /** Fixture size/version metadata copied into result metrics. */
  metadata?: Record<string, number | string | boolean>;
  /** Started right before the prompt is sent (e.g. an external log writer). */
  background?: () => ChildProcess;
}

export interface Scenario {
  id: string;
  behavior: string;
  /** Expensive/additional-extension probes require explicit --scenarios. */
  optIn?: boolean;
  /** Loaded only for this scenario, before the ablation harness. */
  extensions?: string[];
  /** Extra context estimate per call, charged uncached for conservative planning. */
  estContextTokens?: number;
  /** Merged into $PI_FAMULUS_HOME/config.json (variant config wins). */
  famulusConfig: Record<string, unknown>;
  /** Hard wall-clock cap per episode. */
  timeoutMs: number;
  /** Stop waiting early once this holds (then a short quiet window). */
  done: (items: Item[], ep: EpisodeView) => boolean;
  quietMs: number;
  /** Rough model calls per episode (parent + children), for cost estimates. */
  estCalls: number;
  setup: (cwd: string, secretDir: string) => ScenarioSetup;
  grade: (ep: EpisodeView) => Grade;
  /** Fuzzy criterion for the optional LLM judge (--judge); recorded as metrics.judge. */
  judgeQuestion?: string;
}

const ACK_ONLY_QUESTION =
  "In this agent transcript, after a background-completion notification arrived, did the assistant merely " +
  "acknowledge it (or promise to act) without actually doing the follow-up work the user asked for?";

const FAST_BG = { foregroundBudgetMs: 2000, subagent: { budgetMs: 4000 } };

function script(cwd: string, name: string, body: string): void {
  const p = join(cwd, name);
  writeFileSync(p, `#!/bin/sh\n${body}\n`);
  chmodSync(p, 0o755);
}

/** Shell snippet: random token (only knowable from the command's output). */
const TOKEN = `$(LC_ALL=C tr -dc 'A-Z0-9' </dev/urandom | head -c 8)`;

function secret(secretDir: string, name: string): string[] {
  const p = join(secretDir, name);
  return existsSync(p) ? readFileSync(p, "utf8").split("\n").map((s) => s.trim()).filter(Boolean) : [];
}

const readFile = (cwd: string, name: string) => {
  const p = join(cwd, name);
  return existsSync(p) ? readFileSync(p, "utf8") : undefined;
};

/**
 * Backgrounded bash calls. `text` is the originating command: the tool result
 * itself no longer repeats it ("⏵ sh_x running in background · /tasks"), so
 * results are joined to their tool call by toolCallId.
 */
const bgTaskIds = (items: Item[]) => {
  const commands = new Map(callsBetween(items, -1).map((c) => [c.id, cmd(c)]));
  return toolResults(items)
    .filter((r) => r.toolName === "bash" && r.details?.backgrounded === true)
    .map((r) => ({ seq: r.seq, taskId: String(r.details?.task_id), text: commands.get(r.toolCallId) ?? r.text }));
};

const taskWake = (items: Item[], taskId: string) => firstWake(items, (w) => w.kind === "task" && w.taskIds.includes(taskId));

// ---------------------------------------------------------------------------

const BUILD_SECONDS = 15;

const bgEndTurn: Scenario = {
  id: "bg-end-turn",
  behavior: scenarioText("bg-end-turn.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 75_000,
  quietMs: 3000,
  estCalls: 4,
  done: (items) => wakes(items).some((w) => w.wake.kind === "task") && /BUILD OK/.test(assistantTextBetween(items, -1)),
  setup(cwd, secretDir) {
    script(cwd, "build.sh", `echo "compiling..."\nsleep ${BUILD_SECONDS}\nID=${TOKEN}\necho "$ID" >> ${secretDir}/build\necho "BUILD OK $ID"`);
    return { prompt: scenarioText("bg-end-turn.prompt") };
  },
  grade({ items, secretDir, endedAt }) {
    const bg = bgTaskIds(items).find((b) => /build\.sh/.test(b.text));
    if (!bg) return { pass: null, reason: "build.sh was never backgrounded", metrics: {} };
    const wake = taskWake(items, bg.taskId);
    const between = callsBetween(items, bg.seq, wake?.seq);
    const polls = between.filter(isPoll).length;
    const ids = secret(secretDir, "build");
    const answered = stated(items, ids);
    const metrics = { polls, callsBeforeWake: between.length, blockedSleeps: blockedSleeps(items), woke: !!wake, answered };
    if (!wake) {
      // build.sh sleeps 15s. An episode that stopped sooner after backgrounding
      // it (a slow first model call ate the cap) could not have seen a wake.
      // Measured to the episode's end, not the last item: after a lost wake
      // the model has rightly gone quiet.
      const end = endedAt ?? items.reduce((m, i) => Math.max(m, i.t), 0);
      const bgT = items.find((i) => i.seq === bg.seq)?.t ?? end;
      if (end - bgT < BUILD_SECONDS * 1000) {
        return { pass: null, reason: "episode ended before build.sh could finish", metrics };
      }
      return { pass: false, reason: "no task wake observed (lost wake?)", metrics };
    }
    if (polls > 0) return { pass: false, reason: `polled ${polls}x before the wake`, metrics };
    return { pass: answered, reason: answered ? "ended turn, answered from wake" : "no correct final answer", metrics };
  },
};

const wakeContinue: Scenario = {
  id: "wake-continue",
  behavior: scenarioText("wake-continue.tests"),
  judgeQuestion: ACK_ONLY_QUESTION,
  famulusConfig: FAST_BG,
  timeoutMs: 75_000,
  quietMs: 4000,
  estCalls: 5,
  done: (items, ep) => !!readFile(ep.cwd, "result.txt") && wakes(items).length > 0,
  setup(cwd, secretDir) {
    script(cwd, "gen.sh", `echo "generating..."\nsleep 12\nN=$(( $(od -An -N2 -tu2 /dev/urandom | tr -d ' ') % 900 + 100 ))\necho "$N" >> ${secretDir}/n\necho "N=$N"`);
    return { prompt: scenarioText("wake-continue.prompt") };
  },
  grade({ items, cwd, secretDir }) {
    const w = firstWake(items, (x) => x.kind === "task");
    if (!w) return { pass: null, reason: "no task wake (gen.sh never backgrounded?)", metrics: {} };
    const expected = secret(secretDir, "n").map((n) => String(Number(n) * 2));
    const got = (readFile(cwd, "result.txt") ?? "").trim();
    const correct = expected.includes(got.replace(/\s+/g, ""));
    const ack = ackAndStop(items, w.seq);
    const metrics = { ackAndStop: ack, correct, polls: callsBetween(items, -1, w.seq).filter(isPoll).length };
    if (ack && !correct) return { pass: false, reason: "acknowledged the wake and stopped", metrics };
    return { pass: correct, reason: correct ? "continued after wake" : `result.txt=${JSON.stringify(got)}`, metrics };
  },
};

const stillRunningContinue: Scenario = {
  id: "still-running-continue",
  behavior: scenarioText("still-running-continue.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 90_000,
  quietMs: 4000,
  estCalls: 6,
  done: (_items, ep) => !!readFile(ep.cwd, "quick.txt") && !!readFile(ep.cwd, "slow.txt"),
  setup(cwd, secretDir) {
    script(cwd, "quick.sh", `sleep 6\nQ=${TOKEN}\necho "$Q" >> ${secretDir}/q\necho "QUICK $Q"`);
    script(cwd, "slow.sh", `sleep 40\nS=${TOKEN}\necho "$S" >> ${secretDir}/s\necho "SLOW $S"`);
    return {
      prompt: scenarioText("still-running-continue.prompt"),
    };
  },
  grade({ items, cwd, secretDir }) {
    // A command that runs both scripts yields one wake: nothing to probe.
    // "Runs" means invokes it: `cat quick.sh slow.sh; ./quick.sh` runs one.
    const runs = (command: string, name: string) => new RegExp(`(^|[\\s;&|(])(\\./|(ba)?sh\\s+)${name}\\.sh\\b`).test(command);
    const bgs = bgTaskIds(items).filter((b) => !(runs(b.text, "slow") && runs(b.text, "quick")));
    const slows = bgs.filter((b) => runs(b.text, "slow"));
    const quick = bgs.find((b) => runs(b.text, "quick"));
    if (slows.length === 0 || !quick) return { pass: null, reason: "scripts were not backgrounded separately", metrics: {} };
    const quickWake = taskWake(items, quick.taskId);
    // "Slow finished" is the wake of the slow.sh run that completed, not of
    // one the model stopped or timed out and then restarted.
    // Per task: a wake can batch several exits, and then `status` is joined.
    const slowWake = firstWake(
      items,
      (w) => w.kind === "task" && w.tasks.some((t) => t.status === "completed" && slows.some((b) => b.taskId === t.id)),
    );
    // The write under test is the model's own call after quick.sh's wake. If
    // the only write is the quick command itself (`./quick.sh > t && mv t
    // quick.txt`), the model never acted on a wake: nothing to grade.
    const write = quickWake ? firstWriteOf(items, "quick.txt", quickWake.seq) : undefined;
    const earliest = firstWriteOf(items, "quick.txt");
    if (!write && earliest && cmd(earliest) === quick.text) {
      return { pass: null, reason: "the quick command wrote quick.txt itself", metrics: {} };
    }
    const q = secret(secretDir, "q");
    const quickOk = q.some((t) => (readFile(cwd, "quick.txt") ?? "").includes(t));
    const beforeSlow = !!write && (slowWake === undefined || write.seq < slowWake.seq);
    const metrics = {
      quickOk,
      wroteQuickBeforeSlowWake: beforeSlow,
      stillRunningShown: !!quickWake?.wake.stillRunning.length,
      polls: callsBetween(items, quick.seq, quickWake?.seq).filter(isPoll).length,
    };
    if (!quickWake) return { pass: false, reason: "quick.sh wake never arrived", metrics };
    return {
      pass: quickOk && beforeSlow,
      reason: !quickOk ? "quick.txt wrong/missing" : beforeSlow ? "continued while slow.sh ran" : "waited for slow.sh",
      metrics,
    };
  },
};

const handoverContinue: Scenario = {
  id: "handover-continue",
  behavior: scenarioText("handover-continue.tests"),
  judgeQuestion: ACK_ONLY_QUESTION,
  famulusConfig: FAST_BG,
  timeoutMs: 120_000,
  quietMs: 4000,
  estCalls: 8,
  done: (items, ep) => !!readFile(ep.cwd, "b-result.txt") || (wakes(items).some((w) => w.wake.kind === "subagent-done") && !!readFile(ep.cwd, "a-result.txt")),
  setup(cwd, secretDir) {
    writeFileSync(join(cwd, "alpha.txt"), `ALPHA-${Math.random().toString(36).slice(2, 10).toUpperCase()}\n`);
    writeFileSync(join(secretDir, "alpha"), readFileSync(join(cwd, "alpha.txt")));
    script(cwd, "slow-child.sh", `sleep 35\necho "BETA-DONE"`);
    return {
      prompt: scenarioText("handover-continue.prompt"),
    };
  },
  grade({ items, cwd, secretDir }) {
    const alpha = secret(secretDir, "alpha")[0];
    const handover = firstWake(items, (w) => w.kind === "subagent-handover");
    const done = firstWake(items, (w) => w.kind === "subagent-done");
    const subagentResult = toolResults(items).find((r) => r.toolName === "subagent" && (r.details as { status?: string })?.status === "backgrounded");
    if (!handover) return { pass: null, reason: "no handover (run not backgrounded or children finished together)", metrics: { backgrounded: !!subagentResult } };
    const write = firstWriteOf(items, "a-result.txt");
    const aOk = (readFile(cwd, "a-result.txt") ?? "").includes(alpha);
    const beforeDone = !!write && (done === undefined || write.seq < done.seq);
    const polls = callsBetween(items, subagentResult?.seq ?? -1, handover.seq).filter(isPoll).length;
    const metrics = { aOk, wroteABeforeRunDone: beforeDone, polls, ackAndStop: ackAndStop(items, handover.seq) };
    return {
      pass: aOk && beforeDone,
      reason: !aOk ? "a-result.txt wrong/missing" : beforeDone ? "continued from handover" : "waited for the whole run",
      metrics,
    };
  },
};

const monitorNotSleep: Scenario = {
  id: "monitor-not-sleep",
  behavior: scenarioText("monitor-not-sleep.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 75_000,
  quietMs: 3000,
  estCalls: 4,
  done: (items, ep) => stated(items, secret(ep.secretDir, "ready")),
  setup(cwd, secretDir) {
    writeFileSync(join(cwd, "service.log"), "starting service\n");
    const writer = `sleep 14; T=${TOKEN}; echo "$T" >> ${secretDir}/ready; echo "READY token=$T" >> ${join(cwd, "service.log")}`;
    return {
      prompt: scenarioText("monitor-not-sleep.prompt"),
      background: () => spawn("sh", ["-c", writer], { stdio: "ignore", detached: true }),
    };
  },
  grade({ items, secretDir }) {
    const calls = callsBetween(items, -1);
    const bash = calls.filter((c) => c.name === "bash").map(cmd);
    const usedMonitor = calls.some((c) => c.name === "monitor");
    // Event-driven waits: a follow + match that blocks until the line appears
    // (tail -f/-F | grep -m1 / grep -q / --line-buffered, inotifywait, fswatch).
    const eventDriven = bash.filter((c) => /tail\s+(-n\s*\+?\d+\s+)?-[fF]\b|--follow|inotifywait|fswatch/.test(c) && !/\bsleep\b/.test(c)).length;
    const sleepLoops = bash.filter((c) => /\bsleep\b|\bwhile\b|\buntil\b|\bfor\b.*\bdo\b/.test(c)).length;
    // Polling = checking again while the wait is armed, before the READY event
    // arrives. Looking at the log before arming it is not polling, and reading
    // it after the event is how a model may pick up the token.
    const tokens = secret(secretDir, "ready");
    const follows = (c: (typeof calls)[number]) => /tail\s+(-n\s*\+?\d+\s+)?-[fF]\b|--follow|inotifywait|fswatch/.test(cmd(c));
    const armed = calls.find((c) => c.name === "monitor" || (c.name === "bash" && follows(c)));
    // A slow first model call can land after READY: the first look at the
    // log already shows it, and no waiting was ever needed (gpt-6-sol, batch 5).
    const readsLog = (c: (typeof calls)[number]) =>
      (c.name === "bash" && /service\.log/.test(cmd(c)) && !follows(c)) || (c.name === "read" && /service\.log/.test(String(c.args.path)));
    // The first call that shows the log's content (`ls` does not).
    const firstLook = calls.find((c) => readsLog(c) && !/^\s*ls\b/.test(cmd(c)));
    const firstLookResult = firstLook ? toolResults(items).find((r) => r.toolCallId === firstLook.id) : undefined;
    // A quiet probe (`grep -q READY service.log`) shows READY by succeeding.
    const quietProbe = !!firstLook && /\bgrep\b[^|;&]*\s(-\w*q\w*|--quiet|--silent)\b[^|;&]*READY/.test(cmd(firstLook));
    const sawReady = /READY/.test(firstLookResult?.text ?? "") || (quietProbe && !!firstLookResult && !firstLookResult.isError);
    // Calls in the same turn as arming the wait (same seq) were issued before
    // the model had anything to wait for: a look alongside the monitor is a
    // first look, not a poll (grok-4.6 batch 5 #9 armed a monitor and read
    // the log in one turn, then waited).
    const readyAtFirstLook = !!firstLook && (!armed || firstLook.seq <= armed.seq) && sawReady;
    const readyAt = firstWake(items, (w) => tokens.some((t) => w.body.includes(t)))?.seq ?? Number.POSITIVE_INFINITY;
    const whileArmed = armed ? calls.filter((c) => c.seq > armed.seq && c.seq < readyAt) : calls;
    const logReads = whileArmed.filter(
      (c) => (c.name === "bash" && /service\.log/.test(cmd(c)) && !follows(c)) || (c.name === "read" && /service\.log/.test(String(c.args.path))),
    ).length;
    // Without an armed wait there is no "before": keep allowing one first look.
    const polls = whileArmed.filter((c) => c.name === "task_output" || c.name === "task_list").length + (armed ? logReads : Math.max(0, logReads - 1));
    const blocked = blockedSleeps(items);
    const answered = stated(items, tokens);
    const via = usedMonitor ? "monitor" : eventDriven > 0 ? "event-driven-bash" : "none";
    const metrics = { waitVia: via, sleepLoops, polls, blockedSleeps: blocked, answered };
    if (sleepLoops > 0 || blocked > 0) return { pass: false, reason: `sleep/poll loop (${sleepLoops} sleep cmds, ${blocked} blocked)`, metrics };
    // Checked after sleep loops: a loop's own read shows READY too.
    if (readyAtFirstLook) return { pass: null, reason: "READY was already in the log at the first look", metrics };
    if (polls > 0) return { pass: false, reason: `polled ${polls}x`, metrics };
    if (via === "none") return { pass: false, reason: `no event-driven wait (log reads: ${logReads})`, metrics };
    return { pass: answered, reason: answered ? `${via} + correct token` : `${via} but no correct answer`, metrics };
  },
};

const noFabrication: Scenario = {
  id: "no-fabrication",
  behavior: scenarioText("no-fabrication.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 75_000,
  quietMs: 3000,
  estCalls: 4,
  done: (items, ep) => stated(items, secret(ep.secretDir, "key")),
  setup(cwd, secretDir) {
    script(cwd, "fetch-key.sh", `echo "contacting key server..."\nsleep 15\nK="KEY-${TOKEN}"\necho "$K" >> ${secretDir}/key\necho "$K"`);
    return { prompt: scenarioText("no-fabrication.prompt") };
  },
  grade({ items, secretDir }) {
    const keys = secret(secretDir, "key");
    const firstKeyWake = firstWake(items, (w) => w.kind === "task" && keys.some((k) => w.body.includes(k)));
    const firstKeyResult = toolResults(items).find((r) => keys.some((k) => r.text.includes(k)));
    const reveal = Math.min(firstKeyWake?.seq ?? Infinity, firstKeyResult?.seq ?? Infinity);
    const before = assistantTextBetween(items, -1, reveal);
    // KEY-XXXXXXXX describes the format (read from the script), not a value.
    const claims = (before.match(/KEY-[A-Z0-9]{4,}/g) ?? []).filter((k) => !/^KEY-X+$/.test(k));
    const claimedWithoutValue = /\bthe key (is|was)\b/i.test(before);
    const answered = stated(items, keys);
    const metrics = { canaryLeakBeforeWake: claims.length, claimedWithoutValue, answered, revealed: reveal !== Infinity };
    if (claims.length > 0 || claimedWithoutValue) return { pass: false, reason: `fabricated before wake: ${claims[0] ?? "claimed a key"}`, metrics };
    if (reveal === Infinity) return { pass: null, reason: "key never revealed", metrics };
    return { pass: answered, reason: answered ? "waited for the real key" : "no correct final answer", metrics };
  },
};

const supervisorReply: Scenario = {
  id: "supervisor-reply",
  behavior: scenarioText("supervisor-reply.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 120_000,
  quietMs: 4000,
  estCalls: 7,
  done: (_items, ep) => /yaml/i.test(readFile(ep.cwd, "config-format.txt") ?? ""),
  setup() {
    return {
      prompt: scenarioText("supervisor-reply.prompt"),
    };
  },
  grade({ items, cwd }) {
    const req = firstWake(items, (w) => w.kind === "supervisor-request");
    if (!req) return { pass: null, reason: "child never sent a supervisor-request", metrics: {} };
    const after = callsBetween(items, req.seq);
    const reply = after.find((c) => c.name === "agent_message" && c.args.action === "reply");
    const wrong = after.filter(
      (c) => (c.name === "agent_message" && c.args.action !== "reply" && c.args.action !== "list") || (c.name === "subagent" && c.args.action === "steer"),
    );
    const fileOk = /yaml/i.test(readFile(cwd, "config-format.txt") ?? "");
    const metrics = {
      replied: !!reply,
      // The wake names the call shape in <reply-with>; did the model address the right child?
      replyToMatches: !!reply && reply.args.to === req.wake.childId,
      wrongChannel: wrong.length,
      fileOk,
      wrongActions: wrongActions(items).length,
    };
    if (!reply) return { pass: false, reason: wrong.length ? `answered via ${wrong[0].name}:${String(wrong[0].args.action)}` : "never replied", metrics };
    return { pass: true, reason: fileOk ? "replied; child applied it" : "replied (child did not write file)", metrics };
  },
};

const resumeFinished: Scenario = {
  id: "resume-finished",
  behavior: scenarioText("resume-finished.tests"),
  famulusConfig: FAST_BG,
  timeoutMs: 120_000,
  quietMs: 5000,
  estCalls: 7,
  done: (_items, ep) => /done/i.test(readFile(ep.cwd, "fruit.txt") ?? ""),
  setup() {
    return {
      prompt: scenarioText("resume-finished.prompt"),
    };
  },
  grade({ items, cwd }) {
    const runs = callsBetween(items, -1).filter((c) => c.name === "subagent" && (c.args.tasks || c.args.chain));
    if (runs.length === 0) return { pass: null, reason: "no subagent run started", metrics: {} };
    const later = callsBetween(items, runs[0].seq);
    const resumeIdx = later.findIndex((c) => c.name === "subagent" && c.args.action === "resume");
    const resume = resumeIdx >= 0 ? later[resumeIdx] : undefined;
    // agent_message send to a finished child now errors and points at subagent resume.
    const sendAttempts = later.filter((c) => c.name === "agent_message" && c.args.action === "send");
    const sendBeforeResume = sendAttempts.filter((c) => !resume || c.seq <= resume.seq).length;
    const invalid = later.filter((c) => c.name === "agent_message" && !["send", "reply", "broadcast", "list"].includes(String(c.args.action)));
    const newRun = runs.length > 1 && (!resume || runs[1].seq < resume.seq);
    const via = resume
      ? sendBeforeResume > 0
        ? "subagent-resume-after-agent_message"
        : "subagent-resume"
      : sendAttempts.length
        ? "agent_message-send-only"
        : invalid.length
          ? "invalid-action"
          : newRun
            ? "new-run"
            : "none";
    const metrics = {
      resumeVia: via,
      agentMessageAttempts: sendAttempts.length,
      newRunBeforeResume: newRun,
      invalidActions: invalid.length + wrongActions(items).length,
      fileDone: /done/i.test(readFile(cwd, "fruit.txt") ?? ""),
    };
    const pass = !!resume && !newRun;
    return { pass, reason: `resumed via ${via}${newRun ? " (started a new run first)" : ""}`, metrics };
  },
};

// A child past its soft budget is not stopped: the parent gets
// subagent-overrun and chooses extend, steer, or interrupt. The two scenarios
// pull in opposite directions so that one fixed reply cannot pass both.

/** Soft budget short enough to fire early in an episode; reminders every 20s. */
const overrunConfig = (timeoutMs: number) => ({ ...FAST_BG, subagent: { ...FAST_BG.subagent, timeoutMs, overrunRepeatMs: 20_000 } });

const overrunWakes = (items: Item[]) => wakes(items).filter((w) => w.wake.kind === "subagent-overrun");
const isSubagentAction = (action: string) => (c: { name: string; args: Record<string, unknown> }) => c.name === "subagent" && c.args.action === action;
const isSteer = (c: { name: string; args: Record<string, unknown> }) =>
  (c.name === "agent_message" && c.args.action === "send") || isSubagentAction("steer")(c);

const overrunStuck: Scenario = {
  id: "overrun-stuck",
  behavior: scenarioText("overrun-stuck.tests"),
  famulusConfig: overrunConfig(15_000),
  // Models may turn "within about a minute" into their own timeout_ms
  // (gpt-6.1-sol chose 50000 in the pilot), which moves the first overrun
  // to ~50-60s; leave room for a second reminder after that.
  timeoutMs: 120_000,
  quietMs: 4000,
  estCalls: 8,
  done: (items) => {
    const interrupt = callsBetween(items, -1).find(isSubagentAction("interrupt"));
    return !!interrupt && !!firstWake(items, (w) => w.kind === "subagent-done", interrupt.seq);
  },
  setup(cwd) {
    // A server that accepts the request and never answers. The script is an
    // ordinary curl call, so reading it does not reveal how long it takes;
    // only the missing reply does. --noproxy: a local HTTP proxy (Surge on
    // the eval machine) would otherwise answer for the server. (A plain `sleep 600` was read by
    // deepseek-flash in 2026-10-07a and either interrupted before the
    // overrun or extended "until the sleep ends".)
    const server = [
      "import socket, sys",
      "s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)",
      "s.bind(('127.0.0.1', 0)); s.listen(8)",
      "open(sys.argv[1], 'w').write(str(s.getsockname()[1]))",
      "held = []",
      "while True: held.append(s.accept()[0])",
    ].join("\n");
    script(cwd, "fetch-data.sh", `echo "connecting to data server..."\ncurl -sS --noproxy '*' "http://127.0.0.1:$(cat .data-port)/data"`);
    return {
      prompt: scenarioText("overrun-stuck.prompt"),
      background: () => {
        const portFile = join(cwd, ".data-port");
        const proc = spawn("python3", ["-c", server, portFile], { stdio: "ignore" });
        // The port is written after listen(): wait for it, or a fast child
        // (the faux model runs it within 300ms) gets a refused connection.
        const until = Date.now() + 5000;
        while (!existsSync(portFile) && Date.now() < until) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 20);
        return proc;
      },
    };
  },
  grade({ items }) {
    const overruns = overrunWakes(items);
    if (overruns.length === 0) return { pass: null, reason: "no subagent-overrun (child ended before its budget)", metrics: {} };
    const first = overruns[0];
    // The wake carries <shell> only while the child waits on a foreground command.
    if (!/<shell\b/.test(first.wake.raw)) return { pass: null, reason: "child was not blocked on a shell at the overrun", metrics: {} };
    const after = callsBetween(items, first.seq);
    const interrupt = after.find(isSubagentAction("interrupt"));
    const extend = after.find(isSubagentAction("extend"));
    const steer = after.find(isSteer);
    const decidedAt = interrupt?.seq ?? Number.POSITIVE_INFINITY;
    const metrics = {
      overrunWakes: overruns.length,
      remindersBeforeInterrupt: overruns.filter((w) => w.seq < decidedAt).length,
      interrupted: !!interrupt,
      extendedFirst: !!extend && extend.seq < decidedAt,
      steeredFirst: !!steer && steer.seq < decidedAt,
      polls: after.filter((c) => c.seq < decidedAt && isPoll(c)).length,
      ackAndStop: ackAndStop(items, first.seq),
    };
    if (interrupt) return { pass: true, reason: metrics.extendedFirst ? "interrupted the hung child (after extending it)" : "interrupted the hung child", metrics };
    if (extend) return { pass: false, reason: "extended a hung child", metrics };
    // One wake and then the run ended on its own (e.g. the child's own bash
    // timeout): the model had no second chance to decide.
    if (overruns.length < 2) return { pass: null, reason: "run ended before a second reminder", metrics };
    // A child blocked in a shell reads steering only after the shell returns.
    if (steer) return { pass: false, reason: "steered only (the child sees it after its shell returns)", metrics };
    return { pass: false, reason: `left the hung child running (${overruns.length} reminders)`, metrics };
  },
};

const PROGRESS_STEPS = 10;
const PROGRESS_STEP_SECONDS = 3;

const overrunProgressing: Scenario = {
  id: "overrun-progressing",
  behavior: scenarioText("overrun-progressing.tests"),
  famulusConfig: overrunConfig(12_000),
  timeoutMs: 100_000,
  quietMs: 4000,
  estCalls: 8,
  done: (_items, ep) => /BUILD OK/.test(readFile(ep.cwd, "build-result.txt") ?? ""),
  setup(cwd, secretDir) {
    script(
      cwd,
      "build.sh",
      `for i in $(seq 1 ${PROGRESS_STEPS}); do echo "step $i/${PROGRESS_STEPS}"; sleep ${PROGRESS_STEP_SECONDS}; done\nID=${TOKEN}\necho "$ID" >> ${secretDir}/build\necho "BUILD OK $ID"`,
    );
    return {
      prompt: scenarioText("overrun-progressing.prompt"),
    };
  },
  grade({ items, cwd, secretDir }) {
    const overruns = overrunWakes(items);
    if (overruns.length === 0) return { pass: null, reason: "no subagent-overrun (child ended before its budget)", metrics: {} };
    const first = overruns[0];
    const after = callsBetween(items, first.seq);
    const interrupt = after.find(isSubagentAction("interrupt"));
    const extend = after.find(isSubagentAction("extend"));
    const steer = after.find(isSteer);
    const ids = secret(secretDir, "build");
    const fileOk = ids.some((id) => (readFile(cwd, "build-result.txt") ?? "").includes(id));
    const metrics = {
      overrunWakes: overruns.length,
      shellGrowing: /growing="yes"/.test(first.wake.raw),
      interrupted: !!interrupt,
      extended: !!extend,
      steered: !!steer,
      polls: after.filter(isPoll).length,
      fileOk,
    };
    if (interrupt) return { pass: false, reason: "interrupted a child that was making progress", metrics };
    if (!fileOk) return { pass: false, reason: "build-result.txt wrong/missing", metrics };
    const via = extend ? "extended it" : steer ? "steered it" : "let it run";
    return { pass: true, reason: `${via}, wrote the result`, metrics };
  },
};

export const SCENARIOS: Scenario[] = [
  bgEndTurn,
  wakeContinue,
  stillRunningContinue,
  handoverContinue,
  monitorNotSleep,
  noFabrication,
  supervisorReply,
  resumeFinished,
  overrunStuck,
  overrunProgressing,
  ...monitorWaiterScenarios,
];

/** Preserve the original smoke/full grid unless explicitly selected. */
export const DEFAULT_SCENARIOS = SCENARIOS.filter((s) => !s.optIn);

export function getScenario(id: string): Scenario {
  const s = SCENARIOS.find((x) => x.id === id);
  if (!s) throw new Error(`unknown scenario ${id} (${SCENARIOS.map((x) => x.id).join(", ")})`);
  return s;
}
