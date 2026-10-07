/**
 * Programmatic grading primitives over the normalized transcript. All wake
 * knowledge comes from lib/wake-adapter.ts via transcript items.
 */
import { type Item, type ToolCall, toolResults, wakes } from "../lib/transcript.ts";
import type { Wake } from "../lib/wake-adapter.ts";

export interface Grade {
  /** true/false = scored episode; null = invalid (setup precondition not met, excluded from rates). */
  pass: boolean | null;
  reason: string;
  metrics: Record<string, number | string | boolean>;
}

export type CallAt = ToolCall & { seq: number; t: number };

export function callsBetween(items: Item[], fromSeq: number, toSeq = Number.POSITIVE_INFINITY): CallAt[] {
  return items.flatMap((i) =>
    i.kind === "assistant" && i.seq > fromSeq && i.seq < toSeq ? i.toolCalls.map((c) => ({ ...c, seq: i.seq, t: i.t })) : [],
  );
}

export const cmd = (c: ToolCall) => (typeof c.args.command === "string" ? c.args.command : "");

/**
 * Polling = asking about background work instead of waiting for its wake:
 * task_output/task_list, subagent status/get/list, and bash that sleeps,
 * waits, or reads task output files.
 */
export function isPoll(c: ToolCall): boolean {
  if (c.name === "task_output" || c.name === "task_list") return true;
  if (c.name === "subagent" && ["status", "get", "list"].includes(String(c.args.action))) return true;
  if (c.name === "agent_message" && c.args.action === "list") return true;
  if (c.name === "bash") {
    const s = cmd(c);
    return /\bsleep\b|\bwait\b|\.output\b|pi-famulus|\bps\b|pgrep|\/tasks\//.test(s);
  }
  return false;
}

/** Bash tool results rejected by the extension's bare-sleep guard. */
export function blockedSleeps(items: Item[]): number {
  return toolResults(items).filter((r) => r.toolName === "bash" && r.isError && /bare sleep\/idle-loop/.test(r.text)).length;
}

/** Tool results that failed on arguments: schema errors, unknown actions, details.ok === false. */
export function wrongActions(items: Item[]): Array<{ tool: string; text: string }> {
  return toolResults(items)
    .filter(
      (r) =>
        (r.isError && /validation|invalid|must be equal to one of|unknown action|Expected union|requires/i.test(r.text)) ||
        (r.details && (r.details as { ok?: unknown }).ok === false),
    )
    .map((r) => ({ tool: r.toolName, text: r.text.slice(0, 200) }));
}

export function firstWake(items: Item[], pred: (w: Wake) => boolean, afterSeq = -1) {
  return wakes(items).find((w) => w.seq > afterSeq && pred(w.wake));
}

/** Assistant text strictly between two seqs. */
export function assistantTextBetween(items: Item[], fromSeq: number, toSeq = Number.POSITIVE_INFINITY): string {
  return items
    .filter((i) => i.kind === "assistant" && i.seq > fromSeq && i.seq < toSeq)
    .map((i) => (i as { text: string }).text)
    .join("\n");
}

/**
 * Did any assistant text state this canary? Canaries are only knowable once
 * revealed, so the whole transcript is fair game. Checking only the final
 * text failed runs that answered, then acknowledged a later wake (a monitor's
 * exit right after its event): batch 1, 2026-09-29.
 */
export function stated(items: Item[], canaries: string[]): boolean {
  const text = assistantTextBetween(items, -1);
  return canaries.some((c) => text.includes(c));
}

/**
 * Ack-and-stop after a wake: the model's response to the wake (assistant
 * messages up to the next wake or the end) makes no tool call.
 */
export function ackAndStop(items: Item[], wakeSeq: number): boolean {
  const next = wakes(items).find((w) => w.seq > wakeSeq)?.seq ?? Number.POSITIVE_INFINITY;
  const reply = items.filter((i) => i.kind === "assistant" && i.seq > wakeSeq && i.seq < next);
  return reply.length > 0 && reply.every((i) => i.kind === "assistant" && i.toolCalls.length === 0);
}

/** First tool call (at/after seq) that writes `file` via write/edit/bash redirection. */
export function firstWriteOf(items: Item[], file: string, afterSeq = -1): CallAt | undefined {
  const base = file.replace(/^.*\//, "");
  // Any directory prefix: models also write through the absolute cwd path.
  return callsBetween(items, afterSeq).find((c) => {
    if (c.name === "write" || c.name === "edit") return String(c.args.path ?? "").endsWith(base);
    if (c.name === "bash") {
      const b = base.replace(/\./g, "\\.");
      return (
        new RegExp(`>>?\\s*['"]?(\\S*/)?${b}`).test(cmd(c)) ||
        new RegExp(`tee\\s+(-a\\s+)?['"]?(\\S*/)?${b}`).test(cmd(c)) ||
        // mv/cp onto it: a command of its own (at the start or after a
        // separator, not quoted text), the file its last argument; a
        // redirect may follow.
        new RegExp(`(^|[;&|(]\\s*)(mv|cp)\\s+(-\\S+\\s+)*\\S+\\s+['"]?(\\S*/)?${b}['"]?(?=\\s*($|[;&|]|\\d?>))`, "m").test(cmd(c))
      );
    }
    return false;
  });
}
