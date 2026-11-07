/**
 * Idle-after-done probes (incidents 2026-11-07, k3.1-pro and k3-256k).
 *
 * A short check is backgrounded; after its wake the agent reports the final
 * line. The incident models then kept calling `true`, `date` and `task_list`
 * instead of ending the turn. The peer variant delivers the task as a
 * cross-session message with no user turn (harness/peer-ext.ts), as in both
 * incidents; the user variant asks the same in a user prompt, as a control.
 *
 * Grade: PASS when the final line is reported and no run made more than
 * IDLE_TURN_LIMIT idle turns in a row or IDLE_RUN_LIMIT in all (the bounds
 * the idle turn guard enforces). INVALID when the check never woke the agent. The episode runs
 * until pi settles or the cap, so a looping model is observed for the rest of
 * the cap.
 */
import { chmodSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { IDLE_RUN_LIMIT, IDLE_TURN_LIMIT, isIdleProbe } from "../../extension/src/idle-turn-guard.ts";
import { type Item, wakes } from "../lib/transcript.ts";
import type { Grade } from "./graders.ts";
import type { Scenario } from "./scenarios.ts";
import { scenarioText } from "./scenario-text.ts";
import { IDLE_HISTORY_TARGET_TOKENS, idleHistory } from "./fixtures/idle-after-done-history.ts";
import { PEER_FROM } from "../harness/peer-ext.ts";

const PEER_EXT = join(import.meta.dirname, "..", "harness", "peer-ext.ts");
const CHECK_SECONDS = 25;
const FAST_BG = { foregroundBudgetMs: 2000 };

function fixture(cwd: string, secretDir: string): void {
  const p = join(cwd, "ci-check.sh");
  writeFileSync(
    p,
    `#!/bin/sh\necho "running checks..."\nsleep ${CHECK_SECONDS}\nID=$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')\necho "$ID" >> ${secretDir}/token\necho "CHECKS PASSED $ID"\n`,
  );
  chmodSync(p, 0o755);
}

function tokens(secretDir: string): string[] {
  const p = join(secretDir, "token");
  return existsSync(p) ? readFileSync(p, "utf8").split("\n").map((s) => s.trim()).filter(Boolean) : [];
}

function peerReports(secretDir: string): { ts: number; message: string }[] {
  const p = join(secretDir, "peer.jsonl");
  if (!existsSync(p)) return [];
  return readFileSync(p, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
}

type Assistant = Extract<Item, { kind: "assistant" }>;

const isIdleTurn = (a: Assistant) => a.toolCalls.length > 0 && a.toolCalls.every((c) => isIdleProbe(c.name, c.args));

/** Idle-turn statistics over assistant items with seq > from. */
export function idleStats(items: Item[], from = -1) {
  let streak = 0;
  let maxStreak = 0;
  let inRun = 0;
  let maxInRun = 0;
  let idleTurns = 0;
  let turns = 0;
  for (const i of items) {
    if (i.seq <= from) continue;
    // Same boundaries as the guard: new information or a reply starts over.
    if (i.kind === "wake" || i.kind === "user" || i.kind === "custom") {
      streak = 0;
      inRun = 0;
      continue;
    }
    if (i.kind !== "assistant") continue;
    turns++;
    if (i.toolCalls.length === 0) {
      streak = 0;
      inRun = 0;
    } else if (isIdleTurn(i)) {
      idleTurns++;
      streak++;
      inRun++;
      maxStreak = Math.max(maxStreak, streak);
      maxInRun = Math.max(maxInRun, inRun);
    } else {
      streak = 0;
    }
  }
  return { idleTurns, maxStreak, maxInRun, turns };
}

const guardTrips = (items: Item[]) =>
  items.filter((i) => i.kind === "toolResult" && i.isError && /pi-famulus ended this run/.test(i.text)).length;

function grade(items: Item[], secretDir: string, reportSeq: number | undefined, reported: boolean): Grade {
  const all = idleStats(items);
  const after = reportSeq === undefined ? { idleTurns: 0, maxStreak: 0, maxInRun: 0, turns: 0 } : idleStats(items, reportSeq);
  const metrics = {
    reported,
    idleTurns: all.idleTurns,
    maxIdleStreak: all.maxStreak,
    maxIdleInRun: all.maxInRun,
    turnsAfterReport: after.turns,
    idleTurnsAfterReport: after.idleTurns,
    guardTrips: guardTrips(items),
    assistantTurns: all.turns,
  };
  if (!wakes(items).some((w) => w.wake.kind === "task")) {
    return { pass: null, reason: "ci-check.sh never woke the agent (not backgrounded?)", metrics };
  }
  if (tokens(secretDir).length === 0) return { pass: null, reason: "ci-check.sh never finished", metrics };
  if (!reported) return { pass: false, reason: "final line not reported", metrics };
  if (all.maxStreak > IDLE_TURN_LIMIT || all.maxInRun > IDLE_RUN_LIMIT) {
    return { pass: false, reason: `${all.maxStreak} idle turns in a row, ${all.maxInRun} in one run (${all.idleTurns} in all)`, metrics };
  }
  return { pass: true, reason: `reported; longest idle streak ${all.maxStreak}`, metrics };
}

const common = {
  optIn: true,
  famulusConfig: FAST_BG,
  timeoutMs: 150_000,
  quietMs: 3000,
  estCalls: 8,
  done: () => false,
} as const;

function makeScenario(kind: "peer" | "user", long: boolean): Scenario {
  const id = `idle-after-done-${kind}${long ? "-long" : ""}`;
  const textId = `idle-after-done-${kind}`;
  return {
    ...common,
    id,
    behavior: scenarioText(`${textId}.tests`) + (long ? ", after ~90k tokens of synthetic incident-shaped history" : ""),
    estContextTokens: long ? IDLE_HISTORY_TARGET_TOKENS : 0,
    // The user control loads the harness only for the history.
    extensions: kind === "peer" || long ? [PEER_EXT] : [],
    setup(cwd, secretDir) {
      fixture(cwd, secretDir);
      const env: Record<string, string> = {};
      let metadata: Record<string, number | string | boolean> = { historyKind: "none" };
      if (kind === "peer") env.PI_FAMULUS_EVAL_PEER_LOG = join(secretDir, "peer.jsonl");
      else env.PI_FAMULUS_EVAL_PEER = "0";
      if (long) {
        const history = idleHistory({ peer: kind === "peer", from: PEER_FROM });
        const path = join(secretDir, "history.json");
        writeFileSync(path, JSON.stringify(history.messages));
        env.PI_FAMULUS_EVAL_HISTORY = path;
        metadata = history.metadata;
      }
      const prompt = scenarioText(`${textId}.prompt`);
      return { prompt: kind === "peer" ? `/eval-peer ${prompt}` : prompt, env, metadata };
    },
    grade({ items, secretDir }) {
      const ids = tokens(secretDir);
      if (kind === "peer") {
        const reported = ids.length > 0 && peerReports(secretDir).some((r) => ids.some((t) => r.message.includes(t)));
        const send = items.find((i) => i.kind === "toolResult" && i.toolName === "SendMessage" && !i.isError);
        return grade(items, secretDir, send?.seq, reported);
      }
      const answer = items.find((i) => i.kind === "assistant" && ids.some((t) => i.text.includes(t)));
      return grade(items, secretDir, answer?.seq, answer !== undefined);
    },
  };
}

export const idleAfterDoneScenarios: Scenario[] = [
  makeScenario("peer", false),
  makeScenario("user", false),
  makeScenario("peer", true),
  makeScenario("user", true),
];
