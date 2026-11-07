/**
 * Decision-point replay: resume a real pi session from entry <upto> with a
 * real model and count what it does next (harness/replay-ext.ts).
 *
 *   node replay/run-replay.ts <model> <session.jsonl> <upto> <guard:on|off> <label> <outdir>
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

const [model, session, upto, guard, label, outdir] = process.argv.slice(2);
const MAX_TURNS = 12;
const CAP_MS = 180_000;
const sb = createSandbox({ famulusConfig: { idleTurnGuard: guard === "on" } });
const pi = new PiRpc({
  cwd: sb.cwd,
  env: { ...sb.env, REPLAY_SESSION: session, REPLAY_UPTO: upto },
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
const row = { label, model, upto: Number(upto), guard, turns: as.length, idleTurns: idle, maxStreak, endedWithReply: ended, guardTrips, settled, error,
  calls: as.map((a) => a.toolCalls.map((c) => c.name === "bash" ? `bash:${String(c.args.command).slice(0, 30)}` : c.name).join("+") || "(reply)") };
mkdirSync(outdir, { recursive: true });
writeFileSync(join(outdir, `${label}.events.jsonl`), events.map((e) => JSON.stringify(e)).join("\n"));
console.log(JSON.stringify(row));
