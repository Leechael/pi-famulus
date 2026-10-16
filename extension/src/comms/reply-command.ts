import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { Comms } from "./types";

export type ReplyCommandResolution =
  | { childId: string; message: string }
  | { error: string };

export function resolveReplyCommand(
  args: string,
  pending: ReturnType<Comms["pendingRequests"]>,
): ReplyCommandResolution {
  const input = args.trim();
  if (pending.length === 0) return { error: "No pending supervisor decisions." };
  if (!input) {
    return {
      error: `Pending decisions: ${pending.map((item) => `${item.childId} (${item.name})`).join(", ")}`,
    };
  }

  const [target, ...rest] = input.split(/\s+/);
  const matched = pending.filter((item) => item.childId === target || item.name === target);
  if (matched.length === 1) {
    const message = rest.join(" ").trim();
    return message
      ? { childId: matched[0].childId, message }
      : { error: `Usage: /reply ${target} <decision>` };
  }
  if (matched.length > 1) return { error: `Ambiguous pending decision "${target}"; use a child id.` };
  if (pending.length === 1) return { childId: pending[0].childId, message: input };
  return {
    error: `Specify a pending child id or name. Pending: ${pending.map((item) => `${item.childId} (${item.name})`).join(", ")}`,
  };
}

export function registerReplyCommand(pi: ExtensionAPI, comms: Comms): void {
  pi.registerCommand("reply", {
    description: "Reply to a pending subagent decision (/reply <child-id> <decision>)",
    handler: async (args, ctx: ExtensionContext) => {
      const resolved = resolveReplyCommand(args, comms.pendingRequests());
      if ("error" in resolved) {
        ctx.ui.notify(resolved.error, "warning");
        return;
      }
      try {
        comms.reply(resolved.childId, resolved.message);
        ctx.ui.notify(`Replied to ${resolved.childId}.`, "info");
      } catch (err) {
        ctx.ui.notify(err instanceof Error ? err.message : String(err), "error");
      }
    },
  });
}
