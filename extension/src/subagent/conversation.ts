/**
 * Flatten a child AgentSession message list into plain turns.
 * Structural on purpose: this file does not import pi.
 */
import type { ConversationTurn } from "./types";

interface LooseBlock {
  type?: string;
  text?: string;
  thinking?: string;
  name?: string;
  arguments?: unknown;
}

interface LooseMessage {
  role?: string;
  content?: unknown;
  toolName?: string;
  isError?: boolean;
  stopReason?: string;
  errorMessage?: string;
}

function blockText(block: LooseBlock): string {
  if (block.type === "thinking") return block.thinking ?? block.text ?? "";
  if (block.type === "text") return block.text ?? "";
  if (block.type === "toolCall") {
    const args = block.arguments === undefined ? "" : ` ${JSON.stringify(block.arguments)}`;
    return `tool ${block.name ?? "?"}${args}`;
  }
  return "";
}

function visibleUserText(text: string): string {
  let visible = text.replace(/^You are running as model [^\n]+\.\n\n/, "");
  const separator = visible.indexOf("\n\n---\n\n");
  if (separator >= 0) visible = visible.slice(separator + "\n\n---\n\n".length);
  return visible.trim();
}

function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((block) => (block && typeof block === "object" ? blockText(block as LooseBlock) : ""))
    .filter((part) => part.length > 0)
    .join("\n");
}

function emitAssistantBlocks(message: LooseMessage, turns: ConversationTurn[]): void {
  if (!Array.isArray(message.content)) {
    const text = contentText(message.content);
    if (text) turns.push({ role: message.role ?? "message", text });
    return;
  }
  let thinking = "";
  let text = "";
  const flushThinking = () => {
    const t = thinking.trim();
    if (t) turns.push({ role: "thinking", text: t });
    thinking = "";
  };
  const flushText = () => {
    const t = text.trim();
    if (t) turns.push({ role: message.role ?? "assistant", text: t });
    text = "";
  };
  for (const raw of message.content) {
    const block = raw && typeof raw === "object" ? (raw as LooseBlock) : null;
    if (!block?.type) continue;
    if (block.type === "thinking") {
      flushText();
      const part = (block.thinking ?? block.text ?? "").trim();
      if (part) thinking = thinking ? `${thinking}\n\n${part}` : part;
      continue;
    }
    if (block.type === "toolCall") {
      flushThinking();
      flushText();
      const part = blockText(block);
      if (part) turns.push({ role: message.role ?? "assistant", text: part });
      continue;
    }
    if (block.type === "text") {
      flushThinking();
      const part = block.text ?? "";
      if (part) text = text ? `${text}\n${part}` : part;
    }
  }
  flushThinking();
  flushText();
}

export function turnsFromMessages(messages: readonly LooseMessage[]): ConversationTurn[] {
  const turns: ConversationTurn[] = [];
  let firstUserTurn = true;
  for (const message of messages) {
    if (message.role === "system") continue;
    if (message.role === "assistant" && (message.stopReason === "error" || message.stopReason === "aborted")) {
      // A failed model call often has no text; its error is the only record of what happened.
      const text = contentText(message.content);
      if (text) turns.push({ role: "assistant", text });
      turns.push({ role: "assistant error", text: message.errorMessage?.trim() || `model stopped: ${message.stopReason}` });
      continue;
    }
    if (message.role === "toolResult") {
      const name = message.toolName ?? "tool";
      turns.push({ role: message.isError ? `tool ${name} error` : `tool ${name}`, text: contentText(message.content) });
      continue;
    }
    if (message.role === "user") {
      const rawText = contentText(message.content);
      const text = firstUserTurn ? visibleUserText(rawText) : rawText;
      firstUserTurn = false;
      if (text) turns.push({ role: "user", text });
      continue;
    }
    emitAssistantBlocks(message, turns);
  }
  return turns;
}

export function formatConversation(turns: readonly ConversationTurn[]): string {
  if (turns.length === 0) return "(no messages yet)";
  return turns.map((turn) => `── ${turn.role} ──\n${turn.text}`).join("\n\n");
}
