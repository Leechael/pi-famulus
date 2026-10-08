/**
 * Normalize a pi RPC event stream (or a session JSONL) into an ordered list
 * of user-visible transcript items. Wake messages go through wake-adapter.
 */
import type { RpcEvent } from "./rpc.ts";
import { isWakeCustomType, parseWake, type Wake } from "./wake-adapter.ts";

export interface ToolCall {
  id: string;
  name: string;
  args: Record<string, unknown>;
}

export type Item =
  | { kind: "user"; seq: number; t: number; text: string }
  | {
      kind: "assistant";
      seq: number;
      t: number;
      text: string;
      toolCalls: ToolCall[];
      stopReason?: string;
      errorMessage?: string;
      usage?: { input?: number; output?: number; cost?: { total?: number } };
    }
  | {
      kind: "toolResult";
      seq: number;
      t: number;
      toolCallId: string;
      toolName: string;
      text: string;
      details: Record<string, unknown> | undefined;
      isError: boolean;
    }
  | {
      kind: "wake";
      seq: number;
      t: number;
      /** Enclosing raw turn_start seq; absent when delivery-turn evidence is unavailable. */
      turnSeq?: number;
      wake: Wake;
    }
  | { kind: "custom"; seq: number; t: number; customType: string; text: string };

export function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((b: { type?: string; text?: string }) => (b && b.type === "text" ? (b.text ?? "") : ""))
      .join("\n");
  }
  return "";
}

/** Build items from `message_end` events (authoritative final messages). */
export function itemsFromEvents(events: RpcEvent[]): Item[] {
  const items: Item[] = [];
  let turnSeq: number | undefined;
  for (const ev of events) {
    if (ev.type === "turn_start") turnSeq = ev.seq;
    if (ev.type === "turn_end" || ev.type === "agent_start" || ev.type === "agent_end" || ev.type === "agent_settled") turnSeq = undefined;
    if (ev.type !== "message_end") continue;
    const m = ev.message as Record<string, unknown>;
    items.push(...itemFromMessage(m, ev.seq, ev.t).map((item) =>
      item.kind === "wake" && turnSeq !== undefined ? { ...item, turnSeq } : item,
    ));
  }
  return items;
}

export function itemFromMessage(m: Record<string, unknown>, seq: number, t: number): Item[] {
  const role = m.role;
  if (role === "user") return [{ kind: "user", seq, t, text: contentText(m.content) }];
  if (role === "assistant") {
    const content = (m.content as Array<Record<string, unknown>>) ?? [];
    return [
      {
        kind: "assistant",
        seq,
        t,
        text: contentText(content),
        toolCalls: content
          .filter((b) => b.type === "toolCall")
          .map((b) => ({ id: String(b.id), name: String(b.name), args: (b.arguments ?? {}) as Record<string, unknown> })),
        stopReason: m.stopReason as string | undefined,
        errorMessage: m.errorMessage as string | undefined,
        usage: m.usage as never,
      },
    ];
  }
  if (role === "toolResult") {
    return [
      {
        kind: "toolResult",
        seq,
        t,
        toolCallId: String(m.toolCallId),
        toolName: String(m.toolName),
        text: contentText(m.content),
        details: m.details as Record<string, unknown> | undefined,
        isError: m.isError === true,
      },
    ];
  }
  if (role === "custom") {
    const customType = String(m.customType ?? "");
    const text = contentText(m.content);
    if (isWakeCustomType(customType)) return [{ kind: "wake", seq, t, wake: parseWake(customType, text, m.details) }];
    return [{ kind: "custom", seq, t, customType, text }];
  }
  return [];
}

export const wakes = (items: Item[]) =>
  items.filter((i): i is Extract<Item, { kind: "wake" }> => i.kind === "wake");
export const assistants = (items: Item[]) =>
  items.filter((i): i is Extract<Item, { kind: "assistant" }> => i.kind === "assistant");
export const toolResults = (items: Item[]) =>
  items.filter((i): i is Extract<Item, { kind: "toolResult" }> => i.kind === "toolResult");
export const toolCalls = (items: Item[]) =>
  assistants(items).flatMap((a) => a.toolCalls.map((c) => ({ ...c, seq: a.seq, t: a.t })));

/** Did an assistant message follow this item (i.e. the wake got a model turn)? */
export function followedByAssistant(items: Item[], seq: number): boolean {
  return items.some((i) => i.kind === "assistant" && i.seq > seq);
}
