/**
 * comms tools (design doc §4.7):
 *
 * - `contact_supervisor` — registered in child sessions; need_decision blocks
 *   until the parent replies, progress_update is fire-and-forget.
 * - `agent_message` — parent session (and child variant with implicit `from`)
 *   for send / reply / broadcast / list.
 *
 * Zero runtime pi dependency: ToolDefinition is a type-only import.
 */
import { Type } from "typebox";
import { realClock, type Clock } from "../clock";
import type { ToolDefinition } from "@earendil-works/pi-coding-agent";
import type { CommsWithOrigin } from "./comms";
import { assertSiblingAllowed } from "./routing";
import { PROMPTS } from "../prompts.generated";
import type { ChildStatus, Comms, CommsHost, MailboxEntry } from "./types";

export type AgentMessageSender =
  | { kind: "parent" }
  | { kind: "child"; childId: string; runId: string };

// ---------------------------------------------------------------------------
// contact_supervisor (child sessions)
// ---------------------------------------------------------------------------

const contactSupervisorParameters = Type.Object({
  reason: Type.Union([Type.Literal("need_decision"), Type.Literal("progress_update")], {
    description: PROMPTS["tools.child.contact_supervisor.param.reason"],
  }),
  message: Type.String({ description: PROMPTS["tools.child.contact_supervisor.param.message"] }),
});

export interface ContactSupervisorDetails {
  reason: "need_decision" | "progress_update";
  replied: boolean;
  error?: string;
}

export function createContactSupervisorTool(
  comms: Comms,
  childId: string,
): ToolDefinition<typeof contactSupervisorParameters, ContactSupervisorDetails> {
  return {
    name: "contact_supervisor",
    label: "Contact Supervisor",
    description: PROMPTS["tools.child.contact_supervisor.description"],
    promptSnippet: PROMPTS["tools.child.contact_supervisor.snippet"],
    parameters: contactSupervisorParameters,
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      try {
        const text = await comms.contactSupervisor(childId, params.reason, params.message);
        return {
          content: [{ type: "text", text }],
          details: { reason: params.reason, replied: params.reason === "need_decision" },
        };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return {
          content: [{ type: "text", text: `contact_supervisor failed: ${message}` }],
          details: { reason: params.reason, replied: false, error: message },
        };
      }
    },
  };
}

// ---------------------------------------------------------------------------
// agent_message (parent session; child variant with implicit from)
// ---------------------------------------------------------------------------

const agentMessageParameters = Type.Object({
  action: Type.Union(
    [Type.Literal("send"), Type.Literal("reply"), Type.Literal("broadcast"), Type.Literal("list")],
    {
      description: PROMPTS["tools.agent_message.param.action"],
    },
  ),
  to: Type.Optional(
    Type.String({
      description: PROMPTS["tools.agent_message.param.to"],
    }),
  ),
  message: Type.Optional(Type.String({ description: PROMPTS["tools.agent_message.param.message"] })),
  delivery: Type.Optional(
    Type.Union([Type.Literal("steer"), Type.Literal("queue")], {
      description: PROMPTS["tools.agent_message.param.delivery"],
    }),
  ),
});

export interface AgentMessageDetails {
  action: "send" | "reply" | "broadcast" | "list";
  ok: boolean;
  error?: string;
  to?: string;
  delivery?: "steer" | "queue";
  delivered?: string[];
  pending?: { childId: string; name: string; message: string; sinceMs: number }[];
  children?: { childId: string; runId: string; name: string; status: ChildStatus }[];
  log?: MailboxEntry[];
}

type AgentMessageResult = {
  content: { type: "text"; text: string }[];
  details: AgentMessageDetails;
};

function errorResult(
  action: AgentMessageDetails["action"],
  text: string,
  extra: Partial<AgentMessageDetails> = {},
): AgentMessageResult {
  return {
    content: [{ type: "text", text: `Error: ${text}` }],
    details: { action, ok: false, error: text, ...extra },
  };
}

function okResult(
  action: AgentMessageDetails["action"],
  text: string,
  extra: Partial<AgentMessageDetails> = {},
): AgentMessageResult {
  return { content: [{ type: "text", text }], details: { action, ok: true, ...extra } };
}

function truncate(text: string, maxChars = 120): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > maxChars ? `${flat.slice(0, maxChars - 1)}…` : flat;
}

interface ChildRef {
  childId: string;
  runId: string;
  name: string;
  status: ChildStatus;
}

/** Resolve a `to` argument that may be a child_id or a display name. */
function resolveChildRef(host: CommsHost, ref: string): ChildRef | undefined {
  const all = host.listChildren();
  const byId = all.find((c) => c.childId === ref);
  if (byId) return byId;
  const byName = all.filter((c) => c.name === ref);
  if (byName.length > 1) {
    throw new Error(
      `Ambiguous child name "${ref}" matches ${byName.map((c) => c.childId).join(", ")}; ` +
        "use child_id.",
    );
  }
  return byName[0];
}

function knownChildrenText(host: CommsHost): string {
  return host.listChildren().map((c) => `${c.childId} (${c.name})`).join(", ") || "none";
}

function formatListText(
  pending: { childId: string; name: string; message: string; sinceMs: number }[],
  children: ChildRef[],
  entries: MailboxEntry[],
  now: number,
): string {
  const lines: string[] = [];
  lines.push(`Pending decision requests: ${pending.length}`);
  for (const p of pending) {
    const ageS = Math.max(0, Math.round((now - p.sinceMs) / 1000));
    lines.push(`- ${p.childId} (${p.name}) waiting ${ageS}s: "${truncate(p.message)}"`);
  }
  lines.push("", `Children: ${children.length}`);
  for (const c of children) {
    lines.push(`- ${c.childId} [${c.runId}] ${c.name} — ${c.status}`);
  }
  lines.push("", `Recent messages (last ${entries.length}):`);
  for (const e of entries) {
    const reply = e.reply !== undefined ? ` → reply: "${truncate(e.reply, 80)}"` : "";
    lines.push(`- ${e.from} → ${e.to} (${e.kind}): "${truncate(e.message, 80)}"${reply}`);
  }
  return lines.join("\n");
}

/**
 * @param comms  the shared Comms instance
 * @param sender parent session, or the child the tool is registered for
 * @param host   CommsHost — needed for lineage checks, name resolution, and
 *               the `list` summary (deviation from the two-arg sketch in the
 *               task brief; required by the §4.7 routing rules)
 */
export function createAgentMessageTool(
  comms: CommsWithOrigin,
  sender: AgentMessageSender,
  host: CommsHost,
  clock: Clock = realClock,
): ToolDefinition<typeof agentMessageParameters, AgentMessageDetails> {
  const isChild = sender.kind === "child";
  return {
    name: "agent_message",
    label: "Agent Message",
    description: isChild ? PROMPTS["tools.child.agent_message.description"] : PROMPTS["tools.agent_message.description"],
    promptSnippet: isChild ? PROMPTS["tools.child.agent_message.snippet"] : PROMPTS["tools.agent_message.snippet"],
    parameters: agentMessageParameters,
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const action = params.action;
      const from = isChild ? sender.childId : "supervisor";

      if (action === "list") {
        const pending = comms.pendingRequests();
        const children = host.listChildren();
        let entries: MailboxEntry[];
        if (isChild) {
          entries = comms.log(sender.runId, 20);
        } else {
          const runIds = [...new Set(children.map((c) => c.runId))];
          entries = runIds
            .flatMap((runId) => comms.log(runId, 200))
            .sort((a, b) => a.ts - b.ts)
            .slice(-20);
        }
        return okResult("list", formatListText(pending, children, entries, clock.now()), {
          pending,
          children,
          log: entries,
        });
      }

      if (!params.message) {
        return errorResult(action, `action "${action}" requires "message".`);
      }
      const message = params.message;

      if (action === "broadcast") {
        let runId: string;
        let fromChildId: string | undefined;
        if (isChild) {
          if (params.to !== undefined && params.to !== sender.runId) {
            return errorResult("broadcast", "cross-run messaging not allowed");
          }
          runId = sender.runId;
          fromChildId = sender.childId;
        } else {
          if (!params.to) {
            return errorResult("broadcast", 'action "broadcast" requires "to" (run_id).');
          }
          runId = params.to;
        }
        const delivered = await comms.broadcast(runId, message, fromChildId);
        const text =
          delivered.length === 0
            ? `Broadcast to run ${runId}: no running children received it.`
            : `Broadcast to run ${runId} delivered to: ${delivered.join(", ")}.`;
        return okResult("broadcast", text, { delivered });
      }

      // send / reply need a target child.
      if (!params.to) {
        return errorResult(action, `action "${action}" requires "to" (child_id or name).`);
      }
      let target: ChildRef | undefined;
      try {
        target = resolveChildRef(host, params.to);
      } catch (err) {
        return errorResult(action, err instanceof Error ? err.message : String(err));
      }
      if (!target) {
        return errorResult(
          action,
          `Unknown child "${params.to}". Known children: ${knownChildrenText(host)}.`,
        );
      }
      if (isChild) {
        try {
          assertSiblingAllowed(host, sender.childId, target.childId);
        } catch (err) {
          return errorResult(action, err instanceof Error ? err.message : String(err), {
            to: target.childId,
          });
        }
      }

      if (action === "reply") {
        try {
          comms.reply(target.childId, message, from);
        } catch (err) {
          return errorResult("reply", err instanceof Error ? err.message : String(err), {
            to: target.childId,
          });
        }
        return okResult(
          "reply",
          `Replied to ${target.childId} (${target.name}): "${truncate(message, 80)}"`,
          { to: target.childId },
        );
      }

      // send
      const delivery = params.delivery ?? "steer";
      try {
        await comms.send(target.childId, message, delivery, from);
      } catch (err) {
        return errorResult("send", err instanceof Error ? err.message : String(err), {
          to: target.childId,
          delivery,
        });
      }
      const how =
        delivery === "steer" ? "steered into the running child" : "queued for the running child";
      return okResult("send", `Message to ${target.childId} (${target.name}) ${how}.`, {
        to: target.childId,
        delivery,
      });
    },
  };
}
