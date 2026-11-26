/**
 * What did the model do the first time it had to wait? Used by
 * replay/run-replay.ts at decision points where the agent has started (or is
 * about to start) something it needs to watch.
 *
 *   persistent-monitor  a monitor call with persistent:true
 *   timed-monitor       a monitor call without it (timeout_ms or the default)
 *   sleep               bash that sleeps >= SLEEP_MIN_S (a wait, not work)
 *   evasion             bash that waits another way (time.sleep, read -t,
 *                       watch, timeout N tail/grep)
 *   poll                task_output without a byte range, or task_list
 *   end-turn            a reply with no tool call before any of the above
 *   none                none of these within the replayed turns
 *
 * `detached` records a bash launch put in the background with `&`/nohup
 * before the choice: the producer then runs outside any monitor, which the
 * monitor description tells the model not to do.
 */

export const SLEEP_MIN_S = 5;

/** Tool names that start a monitor. */
export const MONITOR_NAMES = new Set(["monitor"]);

export type WaitChoice = "persistent-monitor" | "timed-monitor" | "sleep" | "evasion" | "poll" | "end-turn" | "none";

export interface Turn {
  toolCalls: { name: string; args: Record<string, unknown> }[];
  /** A wait guard refused this turn's bash (its result says why). */
  blocked?: boolean;
}

export interface WaitResult {
  choice: WaitChoice;
  /** 0-based assistant turn of the choice; -1 for none. */
  turn: number;
  detail: string;
  detached: boolean;
}

const SLEEP = /\bsleep\s+(\d+(?:\.\d+)?)/g;
const EVASION = /time\.sleep|\bread\s+-t\b|\bwatch\s+-|\btimeout\s+\d+\s+(tail|grep)\b/;
// A lone `&` (`cmd &`, `(cmd &)`, `(a) & (b)`), nohup, setsid or disown;
// not `&&`, `2>&1` or `&>`.
const DETACH = /(^|[^&>|])&(?![&>])|\bnohup\b|\bsetsid\b|\bdisown\b/m;

export function sleepsToWait(command: string): boolean {
  for (const m of command.matchAll(SLEEP)) if (Number(m[1]) >= SLEEP_MIN_S) return true;
  return false;
}

export function isDetachedLaunch(command: string): boolean {
  return DETACH.test(command);
}

export interface ClassifyOptions {
  /** The extension under test starts a monitor without timeout_ms as persistent. */
  defaultPersistent?: boolean;
}

function isPersistent(args: Record<string, unknown>, opts: ClassifyOptions): boolean {
  if (args.persistent === true) return true;
  return opts.defaultPersistent === true && args.persistent !== false && args.timeout_ms === undefined;
}

export function firstWait(turns: Turn[], opts: ClassifyOptions = {}): WaitResult {
  let detached = false;
  for (let i = 0; i < turns.length; i++) {
    const calls = turns[i].toolCalls;
    if (calls.length === 0) return { choice: "end-turn", turn: i, detail: "", detached };
    for (const c of calls) {
      if (MONITOR_NAMES.has(c.name)) {
        const persistent = isPersistent(c.args, opts);
        return {
          choice: persistent ? "persistent-monitor" : "timed-monitor",
          turn: i,
          detail: persistent ? "persistent" : `timeout_ms=${c.args.timeout_ms ?? "default"}`,
          detached,
        };
      }
      if (c.name === "bash") {
        const command = String(c.args.command ?? "");
        if (sleepsToWait(command)) return { choice: "sleep", turn: i, detail: command.slice(0, 80), detached: detached || isDetachedLaunch(command) };
        if (EVASION.test(command)) return { choice: "evasion", turn: i, detail: command.slice(0, 80), detached };
        if (isDetachedLaunch(command)) detached = true;
        continue;
      }
      if (c.name === "task_list" || (c.name === "task_output" && c.args.cursor === undefined && c.args.offset === undefined)) {
        return { choice: "poll", turn: i, detail: c.name, detached };
      }
    }
  }
  return { choice: "none", turn: -1, detail: "", detached };
}

export type Decision = WaitChoice | "detached" | "work";

/**
 * Only the first replayed turn is decided on the session's real context: the
 * replay runs every bash as `true`, so later turns react to fake results.
 * A turn that launches with `&` and then does not wait is "detached"; one
 * that only does other work is "work" (no decision made).
 */
export function decisionAtTurnZero(turns: Turn[], opts: ClassifyOptions = {}): { decision: Decision; detail: string; detached: boolean } {
  if (turns.length === 0) return { decision: "none", detail: "", detached: false };
  const r = firstWait([turns[0]], opts);
  if (r.choice !== "none") return { decision: r.choice, detail: r.detail, detached: r.detached };
  return r.detached ? { decision: "detached", detail: "", detached: true } : { decision: "work", detail: "", detached: false };
}

/**
 * With a wait guard on, a refused turn is not the decision: the next turns
 * are, after the model has read the refusal. Those turns follow fake results
 * for any other calls of the refused turn (the replay runs bash as `true`),
 * so this is weaker evidence than turn zero. Work after a refusal keeps
 * looking; nothing within the turns is "none".
 */
export function decisionAfterGuard(turns: Turn[], opts: ClassifyOptions = {}): { decision: Decision; turn: number; detail: string; detached: boolean; refusals: number } {
  let refusals = 0;
  for (let i = 0; i < turns.length; i++) {
    if (turns[i].blocked) { refusals++; continue; }
    const d = decisionAtTurnZero([turns[i]], opts);
    if (d.decision === "work" && refusals > 0) continue;
    return { ...d, turn: i, refusals };
  }
  return { decision: "none", turn: -1, detail: "", detached: false, refusals };
}
