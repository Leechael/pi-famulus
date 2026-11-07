/**
 * Generated incident-shaped history for the idle-after-done probes; not a
 * production transcript or replay.
 *
 * Shape of incident A (2026-11-07, see 2026-11-07-IDLE-TURN-REVIEW.md) before its loop: a long delegated
 * investigation (~90k tokens of reads, greps, test runs, edits), a report
 * sent back with SendMessage, a second peer message, CI watched to green, a
 * second report. The probe's own task then arrives as a new peer message.
 * `peer: false` writes the same history with plain user turns and replies in
 * text, for the user-delivered control.
 */
import type { ContextEvent } from "@earendil-works/pi-coding-agent";

type Message = ContextEvent["messages"][number];
type Assistant = Extract<Message, { role: "assistant" }>;

export const IDLE_HISTORY_TARGET_TOKENS = 90_000;
const TARGET_CHARS = IDLE_HISTORY_TARGET_TOKENS * 4;
export const IDLE_HISTORY_VERSION = "idle-after-done-history-v1";

function assistant(content: Assistant["content"], toolUse: boolean): Assistant {
  return {
    role: "assistant", content, api: "synthetic-history", provider: "eval-fixture",
    model: "synthetic-history-not-a-model-response", timestamp: 0,
    usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    stopReason: toolUse ? "toolUse" : "stop",
  };
}

function moduleSource(name: string, n: number): string {
  const lines = [`"""${name}: synthetic module ${n} for the eval fixture."""`, "import logging", "", "logger = logging.getLogger(__name__)", ""];
  for (let f = 0; f < 14; f++) {
    lines.push(`def ${name.replace(/\W/g, "_")}_step_${f}(record, *, attempt=0):`);
    lines.push(`    """Process stage ${f} of ${name}; retries are bounded by RETRY_DELAYS[${f % 5}]."""`);
    lines.push(`    if record.get("status") == "pending_${f}":`);
    lines.push(`        logger.info("module=${n} step=${f} record=%s attempt=%d", record["id"], attempt)`);
    lines.push(`        return {"id": record["id"], "stage": ${f}, "checksum": "${(n * 131 + f * 17).toString(16)}"}`);
    lines.push(`    raise ValueError("unexpected status for step ${f} in ${name}")`);
    lines.push("");
  }
  return lines.join("\n");
}

export function idleHistory(opts: { peer: boolean; from: string }) {
  const messages: Message[] = [];
  let ts = 1;
  let calls = 0;
  let chars = 0;
  const peerMsg = (body: string) =>
    opts.peer ? `<cross-session-message from="${opts.from}">\n${body}\n</cross-session-message>` : body;
  const incoming = (body: string) => {
    messages.push({ role: "user", content: peerMsg(body), timestamp: ts++ });
    chars += body.length;
  };
  const say = (text: string) => messages.push({ ...assistant([{ type: "text", text }], false), timestamp: ts++ });
  const tool = (name: string, args: Record<string, any>, text: string, lead?: string) => {
    const id = `synthetic-${++calls}`;
    const content: Assistant["content"] = lead ? [{ type: "text", text: lead }, { type: "toolCall", id, name, arguments: args }] : [{ type: "toolCall", id, name, arguments: args }];
    messages.push({ ...assistant(content, true), timestamp: ts++ });
    messages.push({ role: "toolResult", toolCallId: id, toolName: name, content: [{ type: "text", text }], isError: false, timestamp: ts++ });
    chars += text.length + JSON.stringify(args).length;
  };
  const report = (body: string) => {
    if (opts.peer) tool("SendMessage", { to: opts.from, message: body, summary: "status report" }, `Queued for "ci-session" (status report).`);
    else say(body);
  };

  incoming("Handoff from the worker session: queue workers restart every ~30 minutes. Please review the plan below, verify it against the code, then carry it out. Plan: raise the queue ack timeout above the longest retry delay instead of shortening the delays.");
  say("I'll verify the retry delays, the broker configuration and the deployment paths before changing anything.");
  const modules = ["jobs/sync", "jobs/notify", "queue_app", "reports/build", "storage/cleanup", "accounts/poll", "exports/finish", "mail/dispatch"];
  let m = 0;
  while (chars < TARGET_CHARS * 0.8) {
    const name = modules[m % modules.length];
    tool("read", { path: `app/${name}.py` }, moduleSource(name, m));
    if (m % 3 === 0) tool("bash", { command: `rg -n "RETRY_DELAYS|countdown|retry\\(" app/${name}.py` }, Array.from({ length: 12 }, (_, i) => `app/${name}.py:${30 + i * 7}:    self.retry(countdown=RETRY_DELAYS[${i % 5}])  # module ${m}`).join("\n"));
    if (m % 5 === 4) tool("bash", { command: `pytest tests/unit/test_${name.replace(/\W/g, "_")}.py -q`, timeout: 600 }, `${"." .repeat(40 + m)}\n${40 + m} passed in ${(2 + m / 10).toFixed(2)}s`);
    m++;
  }
  tool("edit", { path: "docker-compose.yml", edits: [{ oldText: "      - TZ=UTC", newText: "      - TZ=UTC\n      - QUEUE_ACK_TIMEOUT_MS=7200000" }] }, "Successfully replaced 1 block(s) in docker-compose.yml.");
  tool("bash", { command: "git diff --stat" }, " docker-compose.yml | 1 +\n 1 file changed, 1 insertion(+)");
  report("Review and local implementation are complete: the longest retry delay is 3600s, above the 1800s ack timeout. docker-compose.yml now sets the ack timeout to 7200000 ms. Production changes await authorization.");
  say("Reported the review and the local change; waiting for authorization.");
  incoming("Gated steps are done; the owner authorized execution and I ran them. The draft PR is #101. Please watch its CI until green, then report back. Do not mark it ready or merge it.");
  tool("bash", { command: "gh pr checks 101 --watch --interval 30", run_in_background: true }, 'Command "gh pr checks 101 --watch --interval 30" moved to background (task_id: sh_hist0001). You will be notified when it completes.');
  say("Watching CI for PR #101; I'll report when it finishes.");
  incoming('<pi-famulus-wake kind="task"><task id="sh_hist0001" status="completed" exit-code="0"><preview>Detect Changes\tpass\nBackend Tests\tpass\nPrek\tpass</preview></task></pi-famulus-wake>');
  report("PR #101 CI is green; I did not mark it ready or merge it.");
  say("CI for PR #101 is green and reported.");
  while (chars < TARGET_CHARS) {
    const name = modules[m % modules.length];
    tool("read", { path: `app/${name}.py` }, moduleSource(name, m), "Double-checking the remaining retry paths.");
    m++;
  }
  say("All requested work is done and reported.");
  return { messages, metadata: { historyVersion: IDLE_HISTORY_VERSION, historyKind: "synthetic-incident-shaped-not-replay", historyChars: chars, historyMessages: messages.length, historyPeer: opts.peer } };
}
