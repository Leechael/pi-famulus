/**
 * Decision-point replay: resume a real pi session from entry <upto> with a
 * real model and count what it does next (harness/replay-ext.ts).
 *
 *   node replay/run-replay.ts <model> <session.jsonl> <upto> <guard:on|off> <label> <outdir>
 *
 * REPLAY_PROMPTS_FROM=<prompts.generated.ts of the version that wrote the
 * session> swaps its pi-famulus texts for the extension under test's
 * (PI_FAMULUS_EVAL_EXTENSION), so the prompt and the tool definitions match.
 * REPLAY_DUMP=<file> records what reached the request.
 *
 * Prints one JSON row. Session files and outputs stay outside the repo: they
 * hold the user's real work. The replayed context goes to the model's
 * provider again, so replay only sessions that already ran on that provider.
 */
import { writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { createSandbox } from "../lib/sandbox.ts";
import { PiRpc } from "../lib/rpc.ts";
import { itemsFromEvents } from "../lib/transcript.ts";
import { isIdleProbe } from "../../extension/src/idle-turn-guard.ts";
import { EXTENSION_DIR } from "../lib/paths.ts";
import { decisionAtTurnZero, firstWait } from "./wait-choice.ts";

const [model, session, upto, guard, label, outdir] = process.argv.slice(2);
const MAX_TURNS = Number(process.env.REPLAY_MAX_TURNS ?? 12);
const CAP_MS = 180_000;
// REPLAY_WAIT_GUARD=on|off sets the wait guard (default off: replays measure the prompt unless asked).
const sb = createSandbox({ famulusConfig: { idleTurnGuard: guard === "on", waitGuard: process.env.REPLAY_WAIT_GUARD === "on" } });
const pi = new PiRpc({
  cwd: sb.cwd,
  env: {
    ...sb.env,
    REPLAY_SESSION: session,
    REPLAY_UPTO: upto,
    ...(process.env.REPLAY_PROMPTS_FROM
      ? { REPLAY_PROMPTS_FROM: process.env.REPLAY_PROMPTS_FROM, REPLAY_PROMPTS_TO: join(EXTENSION_DIR, "src", "prompts.generated.ts") }
      : {}),
    ...(process.env.REPLAY_DUMP ? { REPLAY_DUMP: process.env.REPLAY_DUMP } : {}),
  },
  model,
  extensions: [join(import.meta.dirname, "..", "harness", "replay-ext.ts")],
});
const t0 = Date.now();
let error: string | undefined;
try {
  await pi.ready(60_000);
  await pi.prompt("/eval-replay");
  for (;;) {
    const turns = itemsFromEvents(pi.events).filter((i) => i.kind === "assistant").length;
    if (turns >= MAX_TURNS || Date.now() - t0 > CAP_MS) break;
    if (turns > 0 && pi.isSettled() && pi.now() - (pi.events.at(-1)?.t ?? 0) > 5000) break;
    await new Promise((r) => setTimeout(r, 500));
  }
} catch (e) {
  error = (e as Error).message;
}
const events = pi.events.slice();
await pi.stop();
sb.cleanup();
const items = itemsFromEvents(events);
const as = items.filter((i) => i.kind === "assistant") as Extract<(typeof items)[number], { kind: "assistant" }>[];
let streak = 0, maxStreak = 0, idle = 0;
for (const a of as) {
  const isIdle = a.toolCalls.length > 0 && a.toolCalls.every((c) => isIdleProbe(c.name, c.args));
  if (isIdle) { idle++; streak++; maxStreak = Math.max(maxStreak, streak); } else streak = 0;
}
const ended = as.length > 0 && as.at(-1)!.toolCalls.length === 0;
const guardTrips = items.filter((i) => i.kind === "toolResult" && /pi-famulus ended this run/.test(i.text)).length;
const settled = as.length < MAX_TURNS && Date.now() - t0 <= CAP_MS + 10_000;
const wait = firstWait(as.map((a) => ({ toolCalls: a.toolCalls.map((c) => ({ name: c.name, args: c.args as Record<string, unknown> })) })));
const row = { label, model, upto: Number(upto), guard, decision: decisionAtTurnZero(as.map((a) => ({ toolCalls: a.toolCalls.map((c) => ({ name: c.name, args: c.args as Record<string, unknown> })) }))), wait, extension: EXTENSION_DIR, turns: as.length, idleTurns: idle, maxStreak, endedWithReply: ended, guardTrips, settled, error,
  calls: as.map((a) => a.toolCalls.map((c) => c.name === "bash" ? `bash:${String(c.args.command).slice(0, 30)}` : c.name).join("+") || "(reply)") };
mkdirSync(outdir, { recursive: true });
writeFileSync(join(outdir, `${label}.events.jsonl`), events.map((e) => JSON.stringify(e)).join("\n"));
console.log(JSON.stringify(row));
