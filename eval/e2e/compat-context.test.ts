/** SDK-protocol integration regression; faux provider only, with no pi-famulus extension or manager. */
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import type { ContextEvent } from "@earendil-works/pi-coding-agent";
import { FIXTURE_VERSION } from "../ablation/fixtures/computer-use-0.5.1.ts";
import { syntheticHistory } from "../ablation/fixtures/monitor-waiter-history.ts";
import { itemFromMessage, toolResults, wakes } from "../lib/transcript.ts";
import { FAUX_EXT } from "../lib/paths.ts";
import { PiRpc } from "../lib/rpc.ts";

const compatibilityExtension = join(import.meta.dirname, "../harness/computer-use-compat-ext.ts");
const tempDirs: string[] = [];
after(() => tempDirs.forEach((dir) => rmSync(dir, { recursive: true, force: true })));
function messageText(message: Record<string, unknown>): string {
  const content = message.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((block: { type?: string; text?: string }) => block.type === "text" ? block.text ?? "" : "").join("\n");
}
function toolCalls(messages: Array<Record<string, unknown>>, name: string, afterIndex = -1): number {
  return messages.slice(afterIndex + 1).reduce((count, message) => {
    if (message.role !== "assistant" || !Array.isArray(message.content)) return count;
    return count + message.content.filter((block: { type?: string; name?: string }) => block.type === "toolCall" && block.name === name).length;
  }, 0);
}

describe("computer-use compatibility context protocol", () => {
  it("preserves the cloned long prefix, native system/tool state, and tool/wake continuations", async () => {
    const root = mkdtempSync(join(tmpdir(), "compat-context-sdk-"));
    tempDirs.push(root);
    const cwd = join(root, "cwd"), secretDir = join(root, "fixture");
    const historyPath = join(secretDir, "history.json"), auditPath = join(secretDir, "audit.jsonl");
    const tracePath = join(root, "trace.jsonl");
    mkdirSync(cwd); mkdirSync(secretDir);
    const history = syntheticHistory(true);
    assert.ok(history.messages.length > 0);
    const historyJson = JSON.stringify(history.messages);
    const expectedMessages = history.messages as ContextEvent["messages"];
    const historyStart = "Watch the local batch-export status log; summarize each checkpoint and keep watching. All batch work below is synthetic and already finished.";
    const historyEnd = "The old export batches are complete; the next user request is a new local watch.";
    writeFileSync(historyPath, historyJson);
    const pi = new PiRpc({
      cwd, includeExtensionUnderTest: false,
      extensions: [FAUX_EXT, compatibilityExtension, join(import.meta.dirname, "../harness/compat-context-wake-ext.ts")], model: "faux/faux-1",
      env: {
        PI_FAMULUS_FAUX_SCRIPT: join(import.meta.dirname, "scripts/compat-context.ts"),
        PI_FAMULUS_FAUX_TRACE: tracePath,
        PI_FAMULUS_COMPAT_FIXTURE: FIXTURE_VERSION, PI_FAMULUS_COMPAT_UI: "1",
        PI_FAMULUS_COMPAT_HISTORY: historyPath, PI_FAMULUS_COMPAT_AUDIT: auditPath,
      },
    });
    try {
      await pi.ready(60_000);
      await pi.prompt("compat-protocol");
      assert.equal(await pi.waitQuiet(100, 30_000), true, pi.stderr.join(""));
      const calls = readFileSync(tracePath, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line) as { messages: Array<Record<string, unknown>> });
      const contexts = calls.map((call) => call.messages);
      const liveToolCounts = contexts.map((messages) => {
        const promptIndex = messages.findLastIndex((message) => message.role === "user" && messageText(message) === "compat-protocol");
        return { promptIndex, observe: toolCalls(messages, "observe_ui", promptIndex), wait: toolCalls(messages, "wait_for", promptIndex), wake: toolCalls(messages, "emit_compat_wake", promptIndex) };
      }).filter((counts) => counts.promptIndex >= 0);
      assert.ok(liveToolCounts.some((counts) => counts.observe === 1), "actual live observe call follows the latest prompt");
      assert.ok(liveToolCounts.some((counts) => counts.wait === 1), "actual live wait call follows the latest prompt, excluding historic waiters");
      assert.ok(liveToolCounts.some((counts) => counts.wake === 1), "actual live synthetic wake call follows the latest prompt");
      assert.ok(contexts.some((messages) => messages.some((message) => message.role === "assistant" && messageText(message).includes("faux compatibility context exercise complete"))));
      const systemRows = contexts.map((messages) => messages.filter((message) => message.role === "system"));
      assert.ok(systemRows.every((rows) => rows.length > 0), "native system prompt remains present for every request");
      assert.ok(systemRows.every((rows) => JSON.stringify(rows) === JSON.stringify(systemRows[0])), "native system prompt is unchanged across continuations");
      for (const messages of contexts) {
        assert.equal(messages.filter((message) => message.role === "user" && messageText(message) === historyStart).length, 1, "history prefix is inserted exactly once per request");
        assert.equal(messages.filter((message) => message.role === "assistant" && messageText(message) === historyEnd).length, 1, "history tail is not cumulatively duplicated");
        const historyEndIndex = messages.findIndex((message) => message.role === "assistant" && messageText(message) === historyEnd);
        const promptIndex = messages.findIndex((message) => message.role === "user" && messageText(message) === "compat-protocol");
        assert.ok(historyEndIndex >= 0 && (promptIndex < 0 || promptIndex > historyEndIndex), "request-local history stays ordered before any live user turn");
      }
      const items = pi.events.filter((event) => event.type === "message_end").flatMap((event) => itemFromMessage(event.message as Record<string, unknown>, event.seq, event.t));
      const fixtureResults = toolResults(items).filter((result) => ["observe_ui", "wait_for", "emit_compat_wake"].includes(result.toolName));
      assert.deepEqual(fixtureResults.filter((result) => result.isError).map((result) => result.text), [], "fixture tools execute through the SDK protocol");
      assert.ok(fixtureResults.some((result) => result.toolName === "wait_for" && result.details?.found === true && typeof result.details?.stateId === "string"), "live wait returns a genuine ready successor state");
      assert.equal(wakes(items).filter((wake) => wake.wake.kind === "monitor" && wake.wake.body === "SDK faux continuation").length, 1, "SDK delivered one synthetic wake continuation without a manager");
      const xmlFallback = itemFromMessage({ role: "custom", customType: "pi-famulus-wake", content: `${(await import("../lib/wake-adapter.ts")).FAMULUS_WAKE_LEAD_IN}\n\n<pi-famulus-wake kind="monitor" id="compat-fixture" description="Synthetic compatibility fixture"><event>SDK faux continuation</event></pi-famulus-wake>` }, 0, 0);
      assert.deepEqual(wakes(xmlFallback)[0]?.wake.taskIds, ["compat-fixture"], "canonical XML fallback retains monitor id when details are absent");
      assert.ok(existsSync(auditPath), "context_with_system must produce audit rows through the actual SDK lifecycle");
      const audit = readFileSync(auditPath, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line) as Record<string, unknown>);
      assert.equal(audit.length, calls.length, "context audit runs on each real SDK provider request, including SDK cache warming");
      assert.ok(audit.every((row) => row.waitForLoaded === true && row.observeUiLoaded === true), "SDK retains captured tool declarations across requests");
      assert.ok(audit.every((row) => Number(row.contextCharsEstimate) >= historyJson.length), "audit includes cached fixture prefix size without reserializing it per request");
      assert.ok(expectedMessages.length > 1, "test exercised the ordered fixture history, not an empty prefix");
      assert.equal(pi.stderr.join("").includes("compat context history is not the leading conversation prefix"), false, pi.stderr.join(""));
    } finally {
      await pi.stop();
    }
  });
});
