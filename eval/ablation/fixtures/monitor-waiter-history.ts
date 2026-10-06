/** Generated benign incident-shaped history; not a production transcript/replay. */
import type { ContextEvent } from "@earendil-works/pi-coding-agent";
import { COMPUTER_USE_VERSION, FIXTURE_VERSION, COMPUTER_USE_SOURCE_SHA256, COMPUTER_USE_BRIDGE_SHA256, WAIT_FOR_SCHEMA_SHA256 } from "./computer-use-0.5.1.ts";

type Message = ContextEvent["messages"][number];
type Assistant = Extract<Message, { role: "assistant" }>;
function historicalAssistant(content: string | Assistant["content"], toolUse = false): Assistant {
  return {
    role: "assistant", content: typeof content === "string" ? [{ type: "text", text: content }] : content,
    api: "synthetic-history", provider: "eval-fixture", model: "synthetic-history-not-a-model-response", timestamp: 0,
    usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    stopReason: toolUse ? "toolUse" : "stop",
  };
}
export const LONG_HISTORY_TARGET_TOKENS = 173_000;
export const LONG_HISTORY_TARGET_CHARS = LONG_HISTORY_TARGET_TOKENS * 4;
export function syntheticHistory(long: boolean) {
  const messages: Message[] = [];
  let timestamp = 1, callCount = 0;
  const user = (text: string) => messages.push({ role: "user", content: text, timestamp: timestamp++ });
  const say = (text: string) => messages.push({ ...historicalAssistant(text), timestamp: timestamp++ });
  const tool = (name: string, args: Record<string, string | number>, text: string, isError = false) => {
    const id = `synthetic-${++callCount}`;
    messages.push({ ...historicalAssistant([{ type: "toolCall", id, name, arguments: args }], true), timestamp: timestamp++ });
    messages.push({ role: "toolResult", toolCallId: id, toolName: name, content: [{ type: "text", text }], isError, timestamp: timestamp++ });
  };
  if (long) {
    user("Watch the local batch-export status log; summarize each checkpoint and keep watching. All batch work below is synthetic and already finished.");
    // Unique, relevant operational detail rather than one giant irrelevant padding block.
    for (let batch = 0; batch < 32; batch++) {
      tool("monitor", { command: "tail -F batch-status.log | grep --line-buffered CHECKPOINT", description: `batch ${batch} checkpoints`, timeout_ms: 1000 }, `Monitor started · task historic_mon_${batch} · timeout 1s\nYou will get a <pi-famulus-wake kind="monitor"> for each event, and a notice when it exits or times out.`);
      user(`<pi-famulus-wake kind="monitor" task-id="historic_mon_${batch}"><event>CHECKPOINT batch=${batch} stage=validated; no next action yet</event></pi-famulus-wake>`);
      const records: string[] = [];
      let chars = 0;
      for (let row = 0; chars < LONG_HISTORY_TARGET_CHARS / 32; row++) {
        const line = JSON.stringify({
          batch, row, record: `export-${batch}-${row}`, stage: ["queued", "validated", "indexed", "ready"][row % 4],
          checks: { schema: "accepted", rows: 100 + row, checksum: `${batch.toString(16)}-${row.toString(16)}-fixture-only`, retry: false },
          summary: "Synthetic local export audit: input accepted, artifacts staged, no remote deployment or UI operation; checkpoint reporting remains event-driven.",
        });
        records.push(line);
        chars += line.length + 1;
      }
      tool("read", { path: "batch-status.log" }, records.join("\n"));
      if ([7, 15, 23, 31].includes(batch)) {
        // Observed incident shape: literal STRING "n", not a schema-invalid number.
        tool("wait_for", { stateId: "n", timeoutMs: 100 }, "State 'n' is unavailable or was evicted. Observe the root again.", true);
        say(`Checkpoint ${batch} recorded; waiting for the next monitor notification.`);
      } else say(`Batch ${batch}: checkpoints recorded; no further work until the next event.`);
      user(`<pi-famulus-wake kind="monitor" task-id="historic_mon_${batch}"><event>Monitor timed out; re-arm only if more work remains.</event></pi-famulus-wake>`);
    }
    say("The old export batches are complete; the next user request is a new local watch.");
  }
  const serialized = JSON.stringify(messages);
  return {
    messages,
    metadata: {
      fixtureVersion: FIXTURE_VERSION, compatibleExtension: COMPUTER_USE_VERSION,
      computerUseSourceSha256: COMPUTER_USE_SOURCE_SHA256, computerUseBridgeSha256: COMPUTER_USE_BRIDGE_SHA256, waitForSchemaSha256: WAIT_FOR_SCHEMA_SHA256,
      historyKind: long ? "synthetic-long-not-incident-replay" : "fresh-short-control",
      historyChars: serialized.length, historyBytes: Buffer.byteLength(serialized),
      historyEstimatedTokens: Math.ceil(serialized.length / 4), historyTokenEstimator: "serialized chars / 4 (not tokenizer; provider usage authoritative)",
      historyMessages: messages.length, historyToolCalls: callCount, historicBogusWaiters: long ? 4 : 0,
      historyTargetTokens: long ? LONG_HISTORY_TARGET_TOKENS : 0,
    },
  };
}
