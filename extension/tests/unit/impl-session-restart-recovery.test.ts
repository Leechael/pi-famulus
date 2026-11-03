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
import { SessionManager } from "@earendil-works/pi-coding-agent";
import { agentRecordPath, writeAgentChildRecord } from "../../src/subagent/agent-records";
import { FAMULUS_WAKE_CUSTOM_TYPE } from "../../src/wake";
import { fakeHostCtx, fakeHostPi, hostedFakeState, resetHostedFakes, settleAsync, startHostedExtension, textOf, until, type SentMessage } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

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

  it("performs fresh same-session recovery after discarding the previous runtime's pending recovery", async () => {
    const sessionId = "sess-fresh-recovery";
    let connect!: () => void;
    hostedFakeState().connectGate = new Promise<void>((resolve) => { connect = resolve; });
    const ext = await startHostedExtension({ sessionId, cwd: home });
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    try {
      let lists = 0;
      ext.manager.list = async () => { lists++; await gate; return []; };
      connect();
      await until(() => lists >= 2, "old background sync and recovery lists");
      await ext.pi.emit("session_shutdown", { reason: "new" }, ext.ctx);
      hostedFakeState().connectGate = null;
      hostedFakeState().tasks = [{
        task_id: "sh_fresh_recovery", session_id: sessionId, kind: "shell", status: "killed", end_reason: "manager-shutdown",
      }];
      const nextCtx = fakeHostCtx(sessionId, home);
      await ext.pi.emit("session_start", { reason: "resume" }, nextCtx);
      release();
      await settleAsync();
      const recoveryWakes = () => ext.pi.sent.filter((sent) =>
        (sent.message.details as { kind?: string })?.kind === "session-recovery" &&
        (sent.message.content ?? "").includes("sh_fresh_recovery"));
      expect(recoveryWakes()).toHaveLength(1);
      // /reload retains this runtime and must not reset its recovery dedup.
      await ext.pi.emit("session_shutdown", { reason: "reload" }, nextCtx);
      await ext.pi.emit("session_start", { reason: "reload" }, nextCtx);
      await settleAsync();
      expect(recoveryWakes()).toHaveLength(1);
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

  it("discards crash recovery already waiting on list when the same runtime reloads", async () => {
    const sessionId = "sess-reload-pending-recovery";
    let connect!: () => void;
    hostedFakeState().connectGate = new Promise<void>((resolve) => { connect = resolve; });
    const ext = await startHostedExtension({ sessionId, cwd: home });
    let activePi = ext.pi;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    try {
      let lists = 0;
      ext.manager.list = async () => {
        lists++;
        await gate;
        return [{ task_id: "sh_reload_history", session_id: sessionId, kind: "shell", status: "killed", end_reason: "manager-shutdown" }];
      };
      connect();
      await until(() => lists >= 2, "background sync and recovery waiting on list");
      await ext.pi.emit("session_shutdown", { reason: "reload" }, ext.ctx);
      activePi = fakeHostPi();
      const { default: extension } = await import("../../src/index");
      extension(activePi.api);
      await activePi.emit("session_start", { reason: "reload" }, ext.ctx);
      release();
      await settleAsync();
      expect(ext.pi.sent).toHaveLength(0);
      expect(activePi.sent).toHaveLength(0);
    } finally {
      connect(); release();
      await activePi.emit("session_shutdown", { reason: "quit" }, ext.ctx);
    }
  });

  it("does not treat a fresh reload runtime as crash recovery", async () => {
    const sessionId = "sess-reload-no-crash";
    writeAgentChildRecord(home, {
      v: 1,
      kind: "agent",
      child_id: "ch_reload",
      run_id: "run_reload",
      session_id: sessionId,
      name: "worker",
      agent: "worker",
      status: "running",
      started_at: Date.now() - 1000,
    });
    hostedFakeState().tasks = [{
      task_id: "sh_reload_killed", session_id: sessionId, kind: "shell", status: "killed", end_reason: "manager-shutdown",
    }];
    const ext = await startHostedExtension({ sessionId, cwd: home, reason: "reload" });
    try {
      await settleAsync();
      expect(ext.pi.sent).toHaveLength(0);
      expect(JSON.parse(readFileSync(agentRecordPath(home, sessionId, "ch_reload"), "utf8")).status).toBe("running");
    } finally {
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

  it("wakes for this session's monitors ended with session-end and still recovers shells", async () => {
    hostedFakeState().tasks = [
      {
        task_id: "mon_ended",
        session_id: SESSION,
        kind: "monitor",
        status: "killed",
        end_reason: "session-end",
      },
      {
        task_id: "sh_crash",
        session_id: SESSION,
        kind: "shell",
        status: "killed",
        end_reason: "manager-crash",
      },
    ];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      await settleAsync();
      await until(() => ext.pi.sent.length > 0, "a recovery notice", 1000).catch(() => {});
      const recovery = ext.pi.sent.filter((entry) => (entry.message.details as { kind?: string })?.kind === "session-recovery");
      const text = recovery.map((entry) => entry.message.content ?? "").join("\n");
      expect({
        count: recovery.length,
        text,
        namesMonitor: text.includes("mon_ended"),
        namesShell: text.includes("sh_crash"),
        mentionsSessionEnd: text.includes("session-end"),
      }).toEqual({
        count: 1,
        text,
        namesMonitor: true,
        namesShell: true,
        mentionsSessionEnd: true,
      });
    } finally {
      await ext.shutdown();
    }
  });

  it("does not recover another session's session-end monitor or a monitor that exited", async () => {
    hostedFakeState().tasks = [
      {
        task_id: "mon_other",
        session_id: "sess-other",
        kind: "monitor",
        status: "killed",
        end_reason: "session-end",
      },
      {
        task_id: "mon_exited",
        session_id: SESSION,
        kind: "monitor",
        status: "completed",
        end_reason: "exited",
      },
    ];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      await settleAsync();
      await new Promise((resolve) => setTimeout(resolve, 300));
      const recovery = ext.pi.sent.filter((entry) => (entry.message.details as { kind?: string })?.kind === "session-recovery");
      const text = recovery.map((entry) => entry.message.content ?? "").join("\n");
      expect({
        recovery,
        text,
        namesOther: text.includes("mon_other"),
        namesExited: text.includes("mon_exited"),
      }).toEqual({
        recovery: [],
        text: "",
        namesOther: false,
        namesExited: false,
      });
    } finally {
      await ext.shutdown();
    }
  });

  function persistSession(): { dir: string; file: string } {
    const dir = mkdtempSync(join(tmpdir(), "famulus-recover-sess-"));
    const sm = SessionManager.create(dir, join(dir, "sessions"));
    sm.appendMessage({ role: "user", content: "seed", timestamp: Date.now() });
    const file = sm.getSessionFile();
    if (!file) throw new Error("session file missing");
    return { dir, file };
  }

  function persistWakes(file: string, sent: SentMessage[]): void {
    const sm = SessionManager.open(file);
    for (const entry of sent) {
      if (entry.message.customType !== FAMULUS_WAKE_CUSTOM_TYPE) continue;
      sm.appendCustomMessageEntry(
        FAMULUS_WAKE_CUSTOM_TYPE,
        entry.message.content ?? "",
        true,
        entry.message.details,
      );
    }
  }

  function recoveryOf(sent: SentMessage[]) {
    return sent.filter((entry) => (entry.message.details as { kind?: string })?.kind === "session-recovery");
  }

  it("second resume omits a monitor id already in session-recovery details", async () => {
    const persisted = persistSession();
    hostedFakeState().tasks = [
      { task_id: "mon_ended", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      { task_id: "sh_crash", session_id: SESSION, kind: "shell", status: "killed", end_reason: "manager-crash" },
    ];
    const first = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      await settleAsync();
      await until(() => first.pi.sent.length > 0, "first recovery", 1000).catch(() => {});
      persistWakes(persisted.file, first.pi.sent);
      await first.shutdown();
      resetHostedFakes();
      hostedFakeState().tasks = [
        { task_id: "mon_ended", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
        { task_id: "sh_crash", session_id: SESSION, kind: "shell", status: "killed", end_reason: "manager-crash" },
      ];
      const second = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file, reason: "resume" });
      try {
        await settleAsync();
        await new Promise((resolve) => setTimeout(resolve, 300));
        const recovery = recoveryOf(second.pi.sent);
        const text = recovery.map((entry) => entry.message.content ?? "").join("\n");
        expect({
          monitorAgain: text.includes("mon_ended"),
          shellAgain: text.includes("sh_crash"),
        }).toEqual({ monitorAgain: false, shellAgain: true });
      } finally {
        await second.shutdown();
      }
    } finally {
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });

  it("live teardown notice then resume does not send a second monitor notice", async () => {
    const persisted = persistSession();
    const first = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      first.manager.start = async () => ({ task_id: "mon_live", pid: 1 });
      await first.pi.tool("monitor").execute(
        "m",
        { command: "tail -F x", description: "live watch", persistent: true },
        undefined,
        undefined,
        first.ctx,
      );
      await first.pi.emit("session_shutdown", { reason: "new" }, first.ctx);
      persistWakes(persisted.file, first.pi.sent);
      resetHostedFakes();
      hostedFakeState().tasks = [
        { task_id: "mon_live", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      ];
      const second = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file, reason: "resume" });
      try {
        await settleAsync();
        await new Promise((resolve) => setTimeout(resolve, 300));
        const text = second.pi.sent.map((entry) => entry.message.content ?? "").join("\n");
        expect(text).not.toContain("mon_live");
      } finally {
        await second.shutdown();
      }
    } finally {
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });

  it("quit then first resume recovers the monitor and the second resume is quiet", async () => {
    const persisted = persistSession();
    hostedFakeState().tasks = [
      { task_id: "mon_ended", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
    ];
    const first = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      await settleAsync();
      await until(() => first.pi.sent.length > 0, "first recovery", 1000).catch(() => {});
      const firstText = recoveryOf(first.pi.sent).map((entry) => entry.message.content ?? "").join("\n");
      expect(firstText).toContain("mon_ended");
      persistWakes(persisted.file, first.pi.sent);
      await first.shutdown();
      resetHostedFakes();
      hostedFakeState().tasks = [
        { task_id: "mon_ended", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      ];
      const second = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file, reason: "resume" });
      try {
        await settleAsync();
        await new Promise((resolve) => setTimeout(resolve, 300));
        expect(recoveryOf(second.pi.sent).some((entry) => (entry.message.content ?? "").includes("mon_ended"))).toBe(false);
      } finally {
        await second.shutdown();
      }
    } finally {
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });

  it("still reports an unrelated monitor id and preserves shell recovery", async () => {
    const persisted = persistSession();
    const sm = SessionManager.open(persisted.file);
    sm.appendCustomMessageEntry(
      FAMULUS_WAKE_CUSTOM_TYPE,
      "prior",
      true,
      { kind: "monitor-session-end", reason: "new", ids: ["mon_old"], monitors: [{ id: "mon_old", description: "old", elapsedMs: 1, outputPath: "/tmp/old" }] },
    );
    hostedFakeState().tasks = [
      { task_id: "mon_old", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      { task_id: "mon_new", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      { task_id: "sh_crash", session_id: SESSION, kind: "shell", status: "killed", end_reason: "manager-crash" },
    ];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      await settleAsync();
      await until(() => ext.pi.sent.length > 0, "recovery", 1000).catch(() => {});
      const recovery = recoveryOf(ext.pi.sent);
      const text = recovery.map((entry) => entry.message.content ?? "").join("\n");
      const details = recovery[0]?.message.details as { monitors?: string[]; tasks?: string[] } | undefined;
      expect({
        text,
        namesOld: text.includes("mon_old"),
        namesNew: text.includes("mon_new"),
        namesShell: text.includes("sh_crash"),
        monitors: details?.monitors,
      }).toEqual({
        text,
        namesOld: false,
        namesNew: true,
        namesShell: true,
        monitors: ["mon_new"],
      });
    } finally {
      await ext.shutdown();
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });

  it("omits a monitor id listed only on monitor-session-end monitors", async () => {
    const persisted = persistSession();
    const sm = SessionManager.open(persisted.file);
    sm.appendCustomMessageEntry(
      FAMULUS_WAKE_CUSTOM_TYPE,
      "prior end",
      true,
      {
        kind: "monitor-session-end",
        reason: "new",
        ids: [],
        monitors: [{ id: "mon_old", description: "old", elapsedMs: 1, outputPath: "/tmp/old" }],
      },
    );
    hostedFakeState().tasks = [
      { task_id: "mon_old", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      { task_id: "mon_new", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
    ];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      await settleAsync();
      await until(() => ext.pi.sent.length > 0, "recovery", 1000).catch(() => {});
      const text = recoveryOf(ext.pi.sent).map((entry) => entry.message.content ?? "").join("\n");
      expect({
        namesOld: text.includes("mon_old"),
        namesNew: text.includes("mon_new"),
      }).toEqual({ namesOld: false, namesNew: true });
    } finally {
      await ext.shutdown();
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });

  it("omits a monitor id listed only in a previous session-recovery tasks array", async () => {
    const persisted = persistSession();
    const sm = SessionManager.open(persisted.file);
    sm.appendCustomMessageEntry(
      FAMULUS_WAKE_CUSTOM_TYPE,
      "prior recovery",
      true,
      { kind: "session-recovery", children: [], tasks: ["mon_old"] },
    );
    hostedFakeState().tasks = [
      { task_id: "mon_old", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
      { task_id: "mon_new", session_id: SESSION, kind: "monitor", status: "killed", end_reason: "session-end" },
    ];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, sessionFile: persisted.file });
    try {
      await settleAsync();
      await until(() => ext.pi.sent.length > 0, "recovery", 1000).catch(() => {});
      const recovery = recoveryOf(ext.pi.sent);
      const text = recovery.map((entry) => entry.message.content ?? "").join("\n");
      expect({
        namesOld: text.includes("mon_old"),
        namesNew: text.includes("mon_new"),
      }).toEqual({ namesOld: false, namesNew: true });
    } finally {
      await ext.shutdown();
      rmSync(persisted.dir, { recursive: true, force: true });
    }
  });
});
