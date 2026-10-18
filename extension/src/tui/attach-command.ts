/**
 * `/attach <child>`: talk to a hosted subagent through the daemon, in the pi TUI.
 * Lines are the child's transcript; Enter sends a prompt (steered while it runs).
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../manager-client";
import type { SubagentRegistry } from "../subagent/registry";
import { fitLines, loadPiTui } from "./pi-tui-load";

export interface AttachCommandDeps {
  getClient: () => ManagerClient | null;
  getRegistry: () => SubagentRegistry | null;
}

export function registerAttachCommand(pi: ExtensionAPI, deps: AttachCommandDeps): void {
  pi.registerCommand("attach", {
    description: "Talk to a hosted subagent (child id)",
    getArgumentCompletions: () => {
      const children = deps.getRegistry()?.list().flatMap((run) => run.children.map((child) => child.childId)) ?? [];
      return children.map((id) => ({ value: id, label: id }));
    },
    handler: async (args, ctx) => {
      const childId = args.trim();
      if (!childId) {
        ctx.ui.notify("Usage: /attach <child id>", "warning");
        return;
      }
      const client = deps.getClient();
      if (!client?.isAvailable() || client.protocolLevel() < 6) {
        ctx.ui.notify("Hosted subagents need a connected pi-famulus daemon (protocol 6).", "error");
        return;
      }
      if (!ctx.hasUI || ctx.mode !== "tui") {
        ctx.ui.notify(`Attach from a terminal with: pi-famulus attach ${childId}`, "info");
        return;
      }
      await openAttachView(ctx, client, childId);
    },
  });
}

interface EntryMessage {
  role?: string;
  content?: string | { text?: string }[];
  message?: EntryMessage;
}

function textOf(message: EntryMessage | undefined): string {
  const content = message?.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => (typeof part === "string" ? part : part?.text ?? "")).join("");
}

function linesFromEntries(entries: unknown): string[] {
  if (!Array.isArray(entries)) return [];
  const out: string[] = [];
  for (const entry of entries as EntryMessage[]) {
    const message = entry.message ?? entry;
    const text = textOf(message);
    if (!text) continue;
    if (message.role === "user") out.push(`you: ${text}`);
    else if (message.role === "assistant") out.push(text);
  }
  return out;
}

/** Live transcript for one hosted child. Closing does not respawn a parked child. */
export async function openAttachView(ctx: ExtensionContext, client: ManagerClient, childId: string): Promise<void> {
  const piTui = loadPiTui();
  if (!piTui) {
    ctx.ui.notify(`pi-famulus attach ${childId}`, "info");
    return;
  }
  await client.agentWatch(childId).catch((err: unknown) => {
    throw new Error(err instanceof Error ? err.message : String(err));
  });
  const seeded = await client.agentInput(childId, "get_entries", "", "parent").catch(() => null);
  const state = await client.agentInput(childId, "get_state", "", "parent").catch(() => null);
  const lines: string[] = [`attached to ${childId}`, ""];
  lines.push(...linesFromEntries(seeded?.entries));
  let input = "";
  let activity = state?.isStreaming === true ? "running" : "idle";
  let status = activity;
  let requestRender: (() => void) | null = null;
  const unsub = client.onEvent((event) => {
    const hosted = event as {
      event?: string;
      child_id?: string;
      record?: { type?: string; message?: { role?: string; content?: { text?: string }[] } };
      activity?: string;
      residency?: string;
      driver?: string;
    };
    if (hosted.child_id !== childId) return;
    if (hosted.event === "agent_state") {
      activity = hosted.activity ?? activity;
      status = `${hosted.activity ?? "?"} · ${hosted.residency ?? "?"} · ${hosted.driver ?? "parent"}`;
      requestRender?.();
      return;
    }
    if (hosted.event !== "agent_rpc" || !hosted.record) return;
    const record = hosted.record;
    if (record.type === "message_end" && record.message?.role === "assistant") {
      const text = textOf(record.message);
      if (text) lines.push(text);
    } else if (record.type === "agent_start") {
      activity = "running";
      lines.push("…");
    }
    requestRender?.();
  });
  try {
    await ctx.ui.custom<void>((tui, _theme, _kb, done) => {
      requestRender = () => tui.requestRender();
      return {
        render(width: number) {
          const limit = Math.max(1, width);
          const rows = Math.max(4, (tui.terminal?.rows ?? 24) - 6);
          const body = lines.flatMap((line) => fitLines(line, limit)).slice(-rows);
          return [
            `◆ ${childId}  ${status}`,
            ...body,
            "",
            `› ${input}`,
            "enter send   esc close",
          ];
        },
        invalidate() {},
        handleInput(data: string) {
          if (data === "\u001b") {
            done();
            return;
          }
          if (data === "\r" || data === "\n") {
            const text = input;
            input = "";
            if (!text.trim()) return;
            lines.push(`you: ${text}`);
            const op = activity === "running" ? "prompt_steer" : "prompt";
            void client.agentInput(childId, op, text, "user").catch((err: unknown) => {
              lines.push(err instanceof Error ? err.message : String(err));
              requestRender?.();
            });
            tui.requestRender();
            return;
          }
          if (data === "\u007f" || data === "\b") {
            input = input.slice(0, -1);
            tui.requestRender();
            return;
          }
          if (data.length === 1 && data >= " ") {
            input += data;
            tui.requestRender();
          }
        },
      };
    });
  } finally {
    unsub();
    await client.agentUnwatch(childId).catch(() => {});
  }
}
