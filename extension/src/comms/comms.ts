/**
 * createComms (design doc §4.7): parent↔child messaging on top of a CommsHost.
 *
 * Supervisor notifications are self-contained here (wiring matrix: comms owns
 * its XML formats; nothing is added to src/format.ts).
 */
import {
  DECISION_TIMEOUT_MESSAGE,
  Mailbox,
  type MailboxOptions,
} from "./mailbox";
import type { Comms, CommsHost } from "./types";
import { fill } from "../prompts.generated";
import { formatFamulusWake, type FormattedWake } from "../wake";
import type { Clock } from "../clock";

/** Ring bucket used when the host does not know the child (defensive fallback). */
export const UNKNOWN_RUN_ID = "unknown";

/** Fire-and-forget progress notification shown to the parent agent. */
export function formatSupervisorUpdate(
  from: { childId: string; name: string },
  message: string,
): FormattedWake {
  return formatFamulusWake({
    kind: "supervisor-update",
    from: from.childId,
    name: from.name,
    message,
  });
}

/** Blocking decision request; the reply recipe is a <reply-with> child, not message text. */
export function formatSupervisorRequest(
  from: { childId: string; name: string },
  message: string,
): FormattedWake {
  return formatFamulusWake({
    kind: "supervisor-request",
    from: from.childId,
    name: from.name,
    message,
  });
}

/**
 * Appendix B `createComms(host)`. The returned object is a `Comms`; the
 * widened `CommsWithOrigin` return type additionally lets callers attribute
 * `send`/`reply` mailbox entries to a child origin (used by the agent_message
 * tool for child senders). Structurally assignable to `Comms`.
 */
export interface CommsWithOrigin extends Comms {
  reply(toChildId: string, message: string, from?: string): void;
  send(
    toChildId: string,
    message: string,
    delivery: "steer" | "queue",
    from?: string,
  ): Promise<void>;
  /** Cancel pending decision waiters and timers (session shutdown). */
  dispose(): void;
}

export interface CommsOptions {
  /** Pre-built mailbox (tests); otherwise one is created from the options below. */
  mailbox?: Mailbox;
  decisionTimeoutMs?: MailboxOptions["decisionTimeoutMs"];
  clock?: Clock;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
}

export function createComms(host: CommsHost, options: CommsOptions = {}): CommsWithOrigin {
  const mailbox =
    options.mailbox ??
    new Mailbox({ decisionTimeoutMs: options.decisionTimeoutMs, clock: options.clock });

  function runIdOf(childId: string): string {
    return host.getChild(childId)?.runId ?? UNKNOWN_RUN_ID;
  }

  return {
    async contactSupervisor(fromChildId, reason, message) {
      const child = host.getChild(fromChildId);
      const runId = child?.runId ?? UNKNOWN_RUN_ID;
      const name = child?.name ?? fromChildId;
      const entry = mailbox.append(runId, {
        from: fromChildId,
        to: "supervisor",
        kind: reason,
        message,
      });

      if (reason === "progress_update") {
        host.notifySupervisor(formatSupervisorUpdate({ childId: fromChildId, name }, message));
        return "ok";
      }

      // need_decision: register the per-child waiter BEFORE notifying, so a
      // supervisor that replies synchronously still resolves correctly.
      const wait = mailbox.beginDecision(fromChildId, name, message);
      options.logEvent?.("decision.request", { child_id: fromChildId });
      const stall = child?.handle as { pauseStall?: () => void; resumeStall?: () => void } | undefined;
      stall?.pauseStall?.();
      host.notifySupervisor(formatSupervisorRequest({ childId: fromChildId, name }, message));
      try {
        const replyText = await wait;
        if (replyText === DECISION_TIMEOUT_MESSAGE) {
          options.logEvent?.("decision.timeout", { child_id: fromChildId });
        }
        entry.reply = replyText;
        return replyText;
      } finally {
        stall?.resumeStall?.();
      }
    },

    reply(toChildId, message, from = "supervisor") {
      if (!mailbox.resolveDecision(toChildId, message)) {
        const pending = mailbox.pendingRequests();
        const listing =
          pending.length === 0
            ? "none"
            : pending.map((p) => `${p.childId} (${p.name})`).join(", ");
        throw new Error(
          `No pending decision request from ${toChildId}. Pending requests: ${listing}.`,
        );
      }
      mailbox.append(runIdOf(toChildId), { from, to: toChildId, kind: "reply", message });
      options.logEvent?.("decision.reply", { child_id: toChildId });
    },

    async send(toChildId, message, delivery, from = "supervisor") {
      const child = host.getChild(toChildId);
      if (!child) {
        const known =
          host.listChildren().map((c) => `${c.childId} (${c.name})`).join(", ") || "none";
        throw new Error(`Unknown child "${toChildId}". Known children: ${known}.`);
      }
      if (child.status === "running") {
        if (delivery === "steer") {
          await child.handle.steer(message);
        } else {
          await child.handle.followUp(message);
        }
      } else if (
        child.status === "completed" ||
        child.status === "failed" ||
        child.status === "interrupted"
      ) {
        // Lifecycle (resume) belongs to the subagent tool. Resuming here never
        // wired a completion wake, so the parent hung.
        throw new Error(
          fill("tools.agent_message.error.finished-child", {
            childId: toChildId,
            name: child.name,
            status: child.status,
            runId: child.runId,
          }),
        );
      } else {
        throw new Error(
          `Child ${toChildId} (${child.name}) is ${child.status}; ` +
            "messages can only be delivered to running or finished children.",
        );
      }
      mailbox.append(child.runId, { from, to: toChildId, kind: "send", message });
    },

    async broadcast(runId, message, fromChildId) {
      const from = fromChildId ?? "supervisor";
      const targets = host
        .listChildren()
        .filter(
          (c) => c.runId === runId && c.status === "running" && c.childId !== fromChildId,
        );
      const delivered: string[] = [];
      for (const target of targets) {
        const child = host.getChild(target.childId);
        if (!child) continue;
        try {
          await child.handle.steer(message);
        } catch {
          continue; // best-effort: only successfully steered children are reported
        }
        delivered.push(target.childId);
        mailbox.append(runId, { from, to: target.childId, kind: "broadcast", message });
      }
      return delivered;
    },

    pendingRequests() {
      return mailbox.pendingRequests();
    },

    log(runId, limit) {
      return mailbox.log(runId, limit);
    },

    dispose() {
      mailbox.dispose();
    },
  };
}