/**
 * The parent should hear early when a
 * child is stuck or going in circles:
 *
 * - the stall watchdog already aborts a silent child and re-prompts it; the
 *   parent should get a light notice when that happens, not only the final
 *   result;
 * - the same tool call with the same arguments three times in a row should
 *   be reported to the parent with the call.
 *
 * Both are judged only by what the parent is sent. The stall settings are
 * the existing `subagent.stallMs` / `stallRetryDelayMs` config. Tool calls
 * arrive as the `tool_execution_start` / `_end` records pi emits in RPC mode,
 * which carry `toolName` and `args`.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { stallRetryPrompt } from "../../src/subagent/runner";
import { resetHostedFakes, settleAsync, startHostedExtension, until, wakeDetails } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const STALL_MS = 300;
const home = mkdtempSync(join(tmpdir(), "pi-famulus-stuck-"));
mkdirSync(home, { recursive: true });
writeFileSync(join(home, "config.json"), JSON.stringify({ subagent: { stallMs: STALL_MS, stallRetryDelayMs: 10 } }));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("a stuck or looping child is reported early", () => {
  it("tells the parent when the stall watchdog restarts a child", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-stall", cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      // Silent past stallMs: the runner aborts and re-prompts the same child.
      await until(
        () => ext.manager.inputs.some((input) => input.op === "prompt" && input.text === stallRetryPrompt(STALL_MS)),
        "the stall retry prompt",
        5_000,
      );
      const beforeDone = ext.pi.sent.map((entry) => entry.message.content ?? "");
      ext.manager.finish(childId, { content: [{ type: "text", text: "recovered" }], stopReason: "stop" });
      await until(() => wakeDetails(ext.pi.sent, "subagent-done").length > 0, "the subagent-done wake");
      expect({
        noticeBeforeDone: beforeDone.some((text) => text.includes(childId) && /stall/i.test(text)),
      }).toEqual({ noticeBeforeDone: true });
    } finally {
      await ext.shutdown();
    }
  });

  it("reports the same tool call repeated three times with the same arguments", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-loop", cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      const before = ext.pi.sent.length;
      for (let i = 1; i <= 3; i++) {
        const toolCallId = `call_${i}`;
        ext.manager.rpc(childId, { type: "tool_execution_start", toolCallId, toolName: "read", args: { path: "src/config.ts" } });
        ext.manager.rpc(childId, { type: "tool_execution_end", toolCallId, toolName: "read", result: { content: [] }, isError: false });
      }
      await settleAsync();
      // Shorter than stallMs, so the stall watchdog cannot end the child meanwhile.
      await until(() => ext.pi.sent.length > before, "a repeat notice", STALL_MS / 3).catch(() => {});
      const after = ext.pi.sent
        .slice(before)
        .filter((entry) => !["subagent-done", "subagent-handover"].includes((entry.message.details as { kind?: string })?.kind ?? ""))
        .map((entry) => entry.message.content ?? "");
      expect({
        notified: after.some((text) => text.includes(childId)),
        namesCall: after.some((text) => text.includes("read") && text.includes("src/config.ts")),
      }).toEqual({ notified: true, namesCall: true });
    } finally {
      await ext.shutdown();
    }
  });
});
