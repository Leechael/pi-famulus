/**
 * `/attach <child>`: talk to a hosted subagent through the daemon, in the pi TUI.
 * Lines are the child's transcript; Enter sends a prompt (steered while it runs).
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../manager-client";
import type { SubagentRegistry } from "../subagent/registry";
import { loadPiTui } from "./pi-tui-load";

export interface AttachCommandDeps {
  getClient: () => ManagerClient | null;
  getRegistry: () => SubagentRegistry | null;
}

export function registerAttachCommand(pi: ExtensionAPI, deps: AttachCommandDeps): void {
  pi.registerCommand("attach", {
    description: "Talk to a hosted subagent (session id / child id)",
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
      await client.agentWatch(childId).catch((err: unknown) => {
        throw new Error(err instanceof Error ? err.message : String(err));
      });
      await openAttach(ctx, client, childId);
    },
  });
}

async function openAttach(ctx: ExtensionContext, client: ManagerClient, childId: string): Promise<void> {
  const piTui = loadPiTui();
  if (!piTui) {
    ctx.ui.notify(`pi-famulus attach ${childId}`, "info");
    return;
  }
  const lines: string[] = [`attached to ${childId}`, ""];
  let input = "";
  let status = "idle";
  const unsub = client.onEvent((event) => {
    const hosted = event as { event?: string; child_id?: string; record?: { type?: string; message?: { role?: string; content?: { text?: string }[] } }; activity?: string; residency?: string; driver?: string };
    if (hosted.child_id !== childId) return;
    if (hosted.event === "agent_state") {
      status = `${hosted.activity ?? "?"} · ${hosted.residency ?? "?"} · ${hosted.driver ?? "parent"}`;
      return;
    }
    if (hosted.event !== "agent_rpc" || !hosted.record) return;
    const record = hosted.record;
    if (record.type === "message_end" && record.message?.role === "assistant") {
      const text = (record.message.content ?? []).map((part) => part.text ?? "").join("");
      if (text) lines.push(text);
    } else if (record.type === "agent_start") {
      lines.push("…");
    }
  });
  try {
    await ctx.ui.custom<void>((tui, _theme, _kb, done) => ({
      render(width: number) {
        const shown = lines.slice(-Math.max(4, (tui.terminal?.rows ?? 24) - 6));
        const body = shown.map((line) => line.slice(0, Math.max(1, width)));
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
          const op = status.startsWith("running") ? "prompt_steer" : "prompt";
          void client.agentInput(childId, op, text, "user").catch((err: unknown) => {
            lines.push(err instanceof Error ? err.message : String(err));
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
    }));
  } finally {
    unsub();
    await client.agentInput(childId, "get_state", "", "parent").catch(() => {});
  }
}
