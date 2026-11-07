/**
 * Wait guard: blocks bash commands that wait instead of watching.
 *
 * In the 2026-11-07 log-monitor session (deepseek-flash) the agent waited in
 * bash 76 times (`sleep 45; check`, `while …; do sleep 5; done`) and started
 * its producers with `(./run.sh > out 2>&1 &)`, then checked on them with
 * `ps`. The monitor description and rules already said not to; a replay of
 * that session at ten decision points (eval/replay) showed that rewording
 * them did not change the choice. The harness has to refuse it.
 *
 * Two shapes are blocked, with a reason that names the alternative:
 *   wait    `sleep N` with N >= WAIT_SLEEP_MIN_S, or any while/until/for loop
 *           that sleeps: use a persistent monitor, or end the turn.
 *   detach  a lone `&`, nohup, setsid or disown: run the producer as the
 *           monitor command, or in the foreground (pi backgrounds it).
 * Quoted strings, heredoc bodies and comments are ignored, so a sleep or `&`
 * inside `ansible -a '…'`, a Python heredoc or a URL does not count.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { fill } from "./prompts.generated.ts";

/** Shorter sleeps settle a restart; longer ones wait for something. */
export const WAIT_SLEEP_MIN_S = 10;

export type WaitShape = { kind: "wait"; what: string } | { kind: "detach"; what: string };

/** Remove what the local shell does not execute as words: heredoc bodies, quotes, comments. */
export function shellWords(command: string): string {
  let s = command.replace(/<<-?\s*(['"]?)(\w+)\1[^\n]*\n[\s\S]*?\n\s*\2\s*(?=\n|$)/g, "<<heredoc");
  s = s.replace(/'[^']*'/g, "''").replace(/"(?:\\.|[^"\\])*"/g, '""');
  s = s.replace(/(^|\s)#[^\n]*/g, "$1");
  return s;
}

const SLEEP = /\bsleep\s+(\d+(?:\.\d+)?)/g;
// A sleep inside a loop body (between `do` and its `done`), not one after the loop.
const SLEEP_LOOP = /\b(while|until|for)\b[\s\S]*?\bdo\b(?:(?!\bdone\b)[\s\S])*\bsleep\b/;
const DETACH = /(^|[^&>|])&(?![&>])|\b(nohup|setsid|disown)\b/m;

export function waitShape(command: string): WaitShape | null {
  const words = shellWords(command);
  if (SLEEP_LOOP.test(words)) return { kind: "wait", what: "a loop that sleeps" };
  for (const m of words.matchAll(SLEEP)) {
    if (Number(m[1]) >= WAIT_SLEEP_MIN_S) return { kind: "wait", what: `sleep ${m[1]}` };
  }
  const d = DETACH.exec(words);
  if (d) return { kind: "detach", what: d[2] ?? "&" };
  return null;
}

export interface WaitGuardOptions {
  enabled: boolean;
  onBlock?: (info: { kind: WaitShape["kind"]; what: string }) => void;
}

export function installWaitGuard(pi: Pick<ExtensionAPI, "on">, opts: WaitGuardOptions): void {
  if (!opts.enabled) return;
  pi.on("tool_call", async (event) => {
    if (event.toolName !== "bash") return;
    const command = (event.input as { command?: unknown } | null)?.command;
    if (typeof command !== "string") return;
    const shape = waitShape(command);
    if (!shape) return;
    opts.onBlock?.({ kind: shape.kind, what: shape.what });
    const reason = fill(shape.kind === "wait" ? "guards.error.wait-sleep" : "guards.error.wait-detach", { what: shape.what });
    return { block: true, reason };
  });
}
