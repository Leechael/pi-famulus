/**
 * Hosted child records and `show` must count tool activity.
 * tool_execution_* events now carry a name, and those
 * turns are written into the conversation so the transcript counter
 * is not stuck at 0.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { agentRecordPath } from "../../src/subagent/agent-records";
import { resetHostedFakes, startHostedExtension, until, wakeDetails } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-tool-activity-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("hosted child tool activity is recorded", () => {
  it("counts named tool calls on the child record", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-tools", cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      ext.manager.rpc(childId, { type: "tool_execution_start", toolCallId: "call_1", toolName: "read", args: { path: "a.txt" } });
      ext.manager.rpc(childId, { type: "tool_execution_end", toolCallId: "call_1", toolName: "read", result: { content: [] }, isError: false });
      ext.manager.rpc(childId, { type: "tool_execution_start", toolCallId: "call_2", toolName: "bash", args: { command: "ls" } });
      ext.manager.rpc(childId, { type: "tool_execution_end", toolCallId: "call_2", toolName: "bash", result: { content: [] }, isError: true });
      ext.manager.finish(childId, { content: [{ type: "text", text: "done" }], stopReason: "stop" });
      await until(() => wakeDetails(ext.pi.sent, "subagent-done").length > 0, "the subagent-done wake");
      const record = JSON.parse(readFileSync(agentRecordPath(home, "sess-tools", childId), "utf8")) as {
        tool_calls?: number;
      };
      expect(record.tool_calls).toBe(2);
    } finally {
      await ext.shutdown();
    }
  });
});
