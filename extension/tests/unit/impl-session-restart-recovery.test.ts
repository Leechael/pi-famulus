/**
 * pi crashes and the user reopens the same session. The new
 * extension instance must tell the model about work the previous process
 * left behind, and must not leave stale records saying running.
 *
 * Left behind here, as after a real crash:
 * - an agent record still `running` (the previous process wrote it and died);
 * - a background command the daemon ended while the session was gone
 *   (`killed`, end reason `manager-shutdown`: the daemon shuts down five
 *   seconds after its last client leaves and kills what is still running).
 *
 * No automatic resume is expected; only a notice and settled records.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { agentRecordPath, writeAgentChildRecord } from "../../src/subagent/agent-records";
import { fakeHostCtx, hostedFakeState, resetHostedFakes, settleAsync, startHostedExtension, textOf, until } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-restart-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

const SESSION = "sess-restart";

describe("the same session reopened after pi crashed", () => {
  it("does not send an old background crash recovery into a replacement session", async () => {
    let connect!: () => void;
    hostedFakeState().connectGate = new Promise<void>((resolve) => { connect = resolve; });
    const ext = await startHostedExtension({ sessionId: "sess-old", cwd: home });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    try {
      let lists = 0;
      ext.manager.list = async () => {
        lists++;
        await gate;
        return [{ task_id: "sh_old_recovery", session_id: "sess-old", kind: "shell", status: "killed", end_reason: "manager-shutdown" }];
      };
      connect();
      await until(() => lists >= 2, "old background sync and recovery lists");
      await ext.pi.emit("session_shutdown", { reason: "new" }, ext.ctx);
      hostedFakeState().connectGate = null;
      const nextCtx = fakeHostCtx("sess-new", home);
      await ext.pi.emit("session_start", { reason: "new" }, nextCtx);
      const before = ext.pi.sent.length;
      release();
      await settleAsync();
      expect(ext.pi.sent.slice(before).some((sent) => (sent.message.content ?? "").includes("sh_old_recovery"))).toBe(false);
    } finally {
      connect(); release();
      await ext.shutdown();
    }
  });

  it("discards a late task_list result even when a new runtime reopens the same session", async () => {
    const sessionId = "sess-sync-generation";
    const ext = await startHostedExtension({ sessionId, cwd: home });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    try {
      await settleAsync();
      let listed = false;
      ext.manager.list = async () => {
        listed = true;
        await gate;
        return [{ task_id: "sh_old_sync", session_id: sessionId, kind: "shell", command: "printf old", status: "completed", exit_code: 0, ended_at: Date.now(), started_at: Date.now() - 10 }];
      };
      const pending = ext.pi.tool("task_list").execute("old-list", {}, undefined, undefined, ext.ctx);
      await until(() => listed, "the old task_list request");
      await ext.pi.emit("session_shutdown", { reason: "new" }, ext.ctx);
      await ext.pi.emit("session_start", { reason: "resume" }, fakeHostCtx(sessionId, home));
      release();
      await pending;
      await new Promise((resolve) => setTimeout(resolve, 300));
      const nextList = await ext.pi.tool("task_list").execute("new-list", {}, undefined, undefined, fakeHostCtx(sessionId, home));
      expect(textOf(nextList)).not.toContain("sh_old_sync");
      expect(ext.pi.sent.some((sent) => (sent.message.content ?? "").includes("sh_old_sync"))).toBe(false);
    } finally {
      release();
      await ext.shutdown();
    }
  });

  it("reports the interrupted subagent and command, and settles the stale record", async () => {
    const startedAt = Date.now() - 10 * 60_000;
    writeAgentChildRecord(home, {
      v: 1,
      kind: "agent",
      child_id: "ch_left",
      run_id: "run_left",
      session_id: SESSION,
      name: "auditor",
      agent: "worker",
      status: "running",
      started_at: startedAt,
      prompt_head: "audit the migrations",
    });
    hostedFakeState().tasks = [{
      task_id: "sh_left",
      session_id: SESSION,
      kind: "shell",
      command: "npm test -- --runInBand",
      cwd: home,
      pid: 999_999,
      status: "killed",
      exit_code: null,
      signal: "SIGTERM",
      started_at: startedAt,
      ended_at: startedAt + 60_000,
      output_path: join(home, "sessions", SESSION, "tasks", "sh_left.output"),
      output_size: 0,
      end_reason: "manager-shutdown",
    }];

    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      await settleAsync();
      await until(() => ext.pi.sent.length > 0, "a recovery notice", 1000).catch(() => {});
      const text = ext.pi.sent.map((entry) => entry.message.content ?? "").join("\n");
      const record = JSON.parse(readFileSync(agentRecordPath(home, SESSION, "ch_left"), "utf8")) as { status: string };
      expect({
        namesSubagent: text.includes("ch_left"),
        namesCommand: text.includes("sh_left"),
        recordStatus: record.status,
      }).toEqual({ namesSubagent: true, namesCommand: true, recordStatus: "interrupted" });
    } finally {
      await ext.shutdown();
    }
  });
});
