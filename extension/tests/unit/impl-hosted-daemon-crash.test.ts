/**
 * The daemon dies while a hosted child is mid-run. After reconnect
 * the child must stop saying running, the parent must get a wake that
 * names it, and resume must be accepted.
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { agentRecordPath } from "../../src/subagent/agent-records";
import { hostedFakeState, resetHostedFakes, settleAsync, startHostedExtension, textOf, until } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-daemon-crash-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

const SESSION = "sess-daemon-crash";

describe("daemon dies while a hosted child runs", () => {
  it("regression: a resumed child waits for its interrupted generation's abort to drain", async () => {
    const sessionId = "sess-interrupt-resume";
    const ext = await startHostedExtension({ sessionId, cwd: home });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      let aborting = false;
      const input = ext.manager.agentInput.bind(ext.manager);
      ext.manager.agentInput = async (id, op, text, source) => {
        if (op === "abort") { aborting = true; await gate; }
        return input(id, op, text, source);
      };
      ext.manager.fireReconnect();
      await until(() => aborting, "the old generation's blocked abort");
      await ext.run({ action: "resume", child_id: childId, message: "continue after crash" });
      await settleAsync();
      const readRecord = () => JSON.parse(readFileSync(agentRecordPath(home, sessionId, childId), "utf8"));
      expect(readRecord().status).toBe("pending");
      expect(ext.manager.inputs.filter((call) => call.text === "continue after crash")).toEqual([]);
      // Restart/abort warmup can report idle before the new prompt starts.
      ext.manager.state(childId, "idle");
      release();
      await until(() => ext.manager.inputs.some((call) => call.text === "continue after crash"), "the resumed prompt");
      expect(readRecord().status).toBe("running");
      ext.manager.finish(childId, { content: [{ type: "text", text: "RESUMED SUCCESS" }], stopReason: "stop" });
      await until(() => readRecord().status === "completed", "the resumed result");
      expect(readRecord().result_tail).toBe("RESUMED SUCCESS");
      expect(ext.pi.sent.filter((sent) => (sent.message.details as { kind?: string })?.kind === "subagent-done").at(-1)?.message.content).toContain("RESUMED SUCCESS");
    } finally {
      release();
      await ext.shutdown();
    }
  });

  it("keeps a locally queued child pending across a same-daemon reconnect", async () => {
    writeFileSync(join(home, "config.json"), JSON.stringify({ subagent: { maxConcurrentChildren: 1 } }));
    const ext = await startHostedExtension({ sessionId: "sess-queued-reconnect", cwd: home });
    try {
      const first = await ext.spawnChild("first", "first");
      hostedFakeState().lookups.set(first, { residency: "warm", activity: "running" });
      await ext.run({ async: true, tasks: [{ prompt: "second", name: "second" }] });
      await settleAsync();
      expect(ext.manager.spawns).toHaveLength(1);
      ext.manager.fireReconnect();
      await settleAsync();
      ext.manager.finish(first, { content: [{ type: "text", text: "first done" }], stopReason: "stop" });
      await until(() => ext.manager.spawns.length === 2 || ext.pi.sent.some((sent) =>
        (sent.message.details as { kind?: string })?.kind === "session-recovery"), "the queued child or a recovery wake");
      expect(ext.pi.sent.filter((sent) => (sent.message.details as { kind?: string })?.kind === "session-recovery")).toEqual([]);
      await until(() => ext.manager.spawns.length === 2, "the queued child to start");
      expect(ext.manager.spawns[1].name).toBe("second");
    } finally {
      await ext.shutdown();
      rmSync(join(home, "config.json"), { force: true });
    }
  });

  it("marks the child interrupted, wakes the parent, and lets it resume", async () => {
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      const childId = await ext.spawnChild();
      const sentBefore = ext.pi.sent.length;
      ext.manager.fireReconnect();
      await until(
        () => ext.pi.sent.slice(sentBefore).some((sent) => (sent.message.content ?? "").includes(childId)),
        "a crash wake that names the child",
      );
      const record = JSON.parse(readFileSync(agentRecordPath(home, SESSION, childId), "utf8")) as { status: string };
      let resume: string;
      try {
        const executed = await ext.run({ action: "resume", child_id: childId, message: "continue" });
        resume = textOf(executed);
      } catch (error) {
        resume = `refused: ${error instanceof Error ? error.message : String(error)}`;
      }
      expect({
        recordStatus: record.status,
        wakeNamesChild: true,
        resumeRefused: resume.startsWith("refused"),
      }).toEqual({
        recordStatus: "interrupted",
        wakeNamesChild: true,
        resumeRefused: false,
      });
    } finally {
      await ext.shutdown();
    }
  });
});
