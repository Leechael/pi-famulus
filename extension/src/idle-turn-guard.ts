/**
 * Idle turn guard: ends a run that keeps making no-op calls.
 *
 * Some models (k3.1-pro, k3-256k on 2026-11-07) cannot end a turn: after the
 * work is done they answer "完成。" and call `true`, `date` or `task_list`,
 * turn after turn, though the prompt says to reply with no tool call. Pi only
 * ends a run on a reply without tool calls, so nothing stopped them.
 *
 * At `message_end` an assistant message whose every tool call is an idle
 * probe is an idle turn. The run ends at IDLE_TURN_LIMIT idle turns in a row
 * or IDLE_RUN_LIMIT in the run: each call of that batch is blocked with
 * `terminate`, so pi ends the run after the batch, as if the model had
 * stopped. A turn with other calls breaks the streak only; a reply without
 * calls, a wake, a user or peer message, or a new run resets both counts.
 * Queued steering still continues the run; wakes still start new ones.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { fill } from "./prompts.generated.ts";

/** Idle turns in a row that end the run. */
export const IDLE_TURN_LIMIT = 2;
/**
 * Idle turns in one run that end it, even with other calls in between. The
 * 2026-11-07 replay interleaved no-ops with fresh read-only checks
 * (`git status`, `gh pr view`, `read`), which reset a consecutive count.
 */
export const IDLE_RUN_LIMIT = 3;

/** Whole commands that do nothing: no pipes, redirects, substitutions or chaining. */
const NOOP_BASH: RegExp[] = [
  /^(true|:|pwd|exit( 0)?)$/,
  /^date(\s+[^;&|<>`$()]*)?$/,
  /^printf(\s+[^;&|<>`$()]*)?$/,
  /^echo(\s+[^;&|<>`$()]*)?$/,
  /^sleep\s+\d+(\.\d+)?[smh]?$/,
];

const STATUS_TOOLS = new Set(["task_list"]);

export function isIdleProbe(toolName: string, args: unknown): boolean {
  if (STATUS_TOOLS.has(toolName)) return true;
  if (toolName !== "bash") return false;
  const command = (args as { command?: unknown } | null)?.command;
  if (typeof command !== "string") return false;
  const trimmed = command.trim();
  return NOOP_BASH.some((re) => re.test(trimmed));
}

function describeCall(name: string, args: unknown): string {
  if (name !== "bash") return name;
  const command = String((args as { command?: unknown }).command ?? "").trim();
  return `bash: ${command.length > 40 ? `${command.slice(0, 39)}…` : command}`;
}

interface ToolCallPart {
  type: "toolCall";
  id: string;
  name: string;
  arguments: unknown;
}

export interface IdleTurnGuardOptions {
  enabled: boolean;
  /** Called once per tripped batch. `calls` lists the idle calls of the streak, oldest first. */
  onTrip?: (info: { count: number; calls: string[] }) => void;
}

export function installIdleTurnGuard(pi: Pick<ExtensionAPI, "on">, opts: IdleTurnGuardOptions): void {
  if (!opts.enabled) return;
  let streak = 0;
  let inRun = 0;
  let idleCalls: string[] = [];
  const blocked = new Map<string, string>();
  const reset = () => {
    streak = 0;
    inRun = 0;
    idleCalls = [];
    blocked.clear();
  };

  // Every run starts clean: whatever started it (a prompt, a wake, a peer
  // message) is new information.
  pi.on("agent_start", async () => reset());

  pi.on("message_end", async (event) => {
    const message = event.message as { role?: string; content?: unknown };
    if (message.role === "toolResult") return;
    const calls = message.role === "assistant" && Array.isArray(message.content)
      ? (message.content as { type?: string }[]).filter((p): p is ToolCallPart => p.type === "toolCall")
      : [];
    if (calls.length === 0) {
      reset();
      return;
    }
    if (!calls.every((c) => isIdleProbe(c.name, c.arguments))) {
      streak = 0;
      return;
    }
    streak += 1;
    inRun += 1;
    idleCalls.push(...calls.map((c) => describeCall(c.name, c.arguments)));
    if (streak < IDLE_TURN_LIMIT && inRun < IDLE_RUN_LIMIT) return;
    const shown = idleCalls.slice(-4);
    const reason = streak >= IDLE_TURN_LIMIT
      ? fill("guards.error.idle-turn", { count: String(streak), calls: shown.join("; ") })
      : fill("guards.error.idle-run", { count: String(inRun), calls: shown.join("; ") });
    for (const c of calls) blocked.set(c.id, reason);
    opts.onTrip?.({ count: Math.max(streak, inRun), calls: shown });
  });

  pi.on("tool_call", async (event) => {
    if (event.parentToolCallId) return;
    const reason = blocked.get(event.toolCallId);
    if (reason === undefined) return;
    blocked.delete(event.toolCallId);
    return { block: true, reason, terminate: true };
  });
}
