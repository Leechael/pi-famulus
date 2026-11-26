/**
 * Peer-delegation harness for the idle-after-done probes. Load it with the
 * extension under test (scenario `extensions`), only for those probes.
 *
 * It reproduces what a session started by another agent sees, without a
 * relay: the episode prompt is the command `/eval-peer <body>`, which delivers
 * <body> as a `pi-famulus-cross-session` message (no user message), and the
 * system prompt gets the extension's own cross-session section. SendMessage is
 * a stub with the real tool's description and parameters; each call is
 * appended to $PI_FAMULUS_EVAL_PEER_LOG and answered like a queued send.
 *
 * Not reproduced: receipts, read/delivered notes, replies from the peer.
 *
 * PI_FAMULUS_EVAL_HISTORY=<json file> prepends synthetic prior messages to
 * every request; with PI_FAMULUS_EVAL_PEER=0 only that part is active (the
 * user-delivered control).
 */
import { appendFileSync, readFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { buildEnvelope, CROSS_SESSION_PROMPT, SEND_MESSAGE_DESCRIPTION, SEND_MESSAGE_JSON_SCHEMA } from "../../extension/src/intercom/surface.ts";

export const PEER_FROM = "claude/5d0c3b0e-7f1a-4c2e-9a51-3e8f0b6d2c47";
export const PEER_NAME = "ci-session";

export default function peerHarness(pi: ExtensionAPI): void {
  const log = process.env.PI_FAMULUS_EVAL_PEER_LOG;
  // Optional synthetic prior history, prepended per request (never stored).
  const historyPath = process.env.PI_FAMULUS_EVAL_HISTORY;
  const history = historyPath ? JSON.parse(readFileSync(historyPath, "utf8")) : [];
  if (history.length) pi.on("context", (event) => ({ messages: [...history, ...event.messages] }));
  if (process.env.PI_FAMULUS_EVAL_PEER === "0") return;

  pi.on("before_agent_start", async (event) => ({ systemPrompt: `${event.systemPrompt}\n\n${CROSS_SESSION_PROMPT}` }));

  pi.registerCommand("eval-peer", {
    description: "eval: deliver the argument as a cross-session message",
    handler: async (args) => {
      pi.sendMessage(
        {
          customType: "pi-famulus-cross-session",
          content: buildEnvelope(PEER_FROM, args),
          display: true,
          details: { from: PEER_FROM, fromSession: "", body: args },
        },
        { deliverAs: "steer", triggerTurn: true },
      );
    },
  });

  pi.registerTool({
    name: "SendMessage",
    label: "SendMessage",
    description: SEND_MESSAGE_DESCRIPTION,
    promptSnippet: "Message other agent sessions via SendMessage.",
    parameters: SEND_MESSAGE_JSON_SCHEMA as any,
    async execute(_id, params): Promise<any> {
      const p = params as { to?: string; message?: string; summary?: string };
      const to = String(p.to ?? "").trim();
      const message = String(p.message ?? "");
      const summary = String(p.summary ?? "").trim();
      if (!to) return { content: [{ type: "text", text: "Error: 'to' is required." }], details: {}, isError: true };
      if (!message) return { content: [{ type: "text", text: "Error: 'message' or notify_when_idle: true is required." }], details: {}, isError: true };
      if (to !== PEER_FROM && to !== PEER_NAME) {
        return { content: [{ type: "text", text: `Error: no session matches "${to}".` }], details: {}, isError: true };
      }
      if (log) appendFileSync(log, `${JSON.stringify({ ts: Date.now(), to, message, summary })}\n`);
      const text = summary ? `Queued for "${PEER_NAME}" (${summary}).` : `Queued for "${PEER_NAME}".`;
      return { content: [{ type: "text", text }], details: { to: PEER_NAME } };
    },
  });
}
