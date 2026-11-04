import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import type { ManagerClient } from "../../src/manager-client";
import { writeAgentChildRecord } from "../../src/subagent/agent-records";
import { createTaskListTool } from "../../src/task-tools";
import { WorkIndex } from "../../src/work-index";

const dirs: string[] = [];
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

function record(sessionId: string, childId: string, status: "running" | "completed") {
  return {
    v: 1 as const,
    kind: "agent" as const,
    child_id: childId,
    run_id: `run_${childId}`,
    session_id: sessionId,
    name: childId,
    agent: "worker",
    status,
    started_at: 0,
    ...(status === "completed" ? { ended_at: 10 } : {}),
  };
}

// Manual testing (2026-09-24): asking the agent "what tasks are running"
// surfaced work other pi sessions had started. The model only ever sees and
// manages what this session created.
describe("task_list scope", () => {
  async function list(all: boolean): Promise<string> {
    const home = mkdtempSync(join(tmpdir(), "pi-famulus-scope-"));
    dirs.push(home);
    writeAgentChildRecord(home, record("sess-mine", "ch_mine_done", "completed"));
    writeAgentChildRecord(home, record("sess-mine", "ch_mine_live", "running"));
    // Left "running" on disk by an earlier pi process of this session.
    writeAgentChildRecord(home, record("sess-mine", "ch_mine_ghost", "running"));
    writeAgentChildRecord(home, record("sess-other", "ch_other_live", "running"));
    writeAgentChildRecord(home, record("sess-other", "ch_other_done", "completed"));
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      list: async () => [],
      sessions: async () => [
        { session_id: "sess-mine", connected: true },
        { session_id: "sess-other", connected: true },
      ],
    } as unknown as ManagerClient;
    const clock = new ManualClock(100);
    const index = new WorkIndex({ clock });
    index.upsert({ id: "ch_mine_live", kind: "agent", status: "running", title: "mine", startedAt: 0, countsAsWorker: false });
    const tool = createTaskListTool({ getClient: () => client, getIndex: () => index, home, sessionId: () => "sess-mine", clock });
    const res = await tool.execute("t", { all }, undefined as never, undefined as never, {} as never);
    return (res.content[0] as { text: string }).text;
  }

  it("default lists only this session's live work", async () => {
    const text = await list(false);
    expect(text).toContain("ch_mine_live");
    expect(text).not.toContain("ch_mine_done");
    expect(text).not.toContain("ch_mine_ghost");
    expect(text).not.toContain("ch_other");
  });

  it("all adds finished work, still only from this session", async () => {
    const text = await list(true);
    expect(text).toContain("ch_mine_live");
    expect(text).toContain("ch_mine_done");
    expect(text).toMatch(/ch_mine_ghost \[agent\] interrupted/);
    expect(text).not.toContain("ch_other");
    expect(text).not.toMatch(/other session/i);
  });

  it("lists a warm hosted child as running after the parent process is gone", async () => {
    const home = mkdtempSync(join(tmpdir(), "pi-famulus-scope-"));
    dirs.push(home);
    writeAgentChildRecord(home, record("sess-mine", "ch_hosted_live", "running"));
    writeFileSync(join(home, "manager.pid"), JSON.stringify({ pid: process.pid }));
    mkdirSync(join(home, "agents"), { recursive: true });
    writeFileSync(
      join(home, "agents", "ch_hosted_live.json"),
      JSON.stringify({
        child_id: "ch_hosted_live",
        parent_session: "sess-mine",
        residency: "warm",
        activity: "running",
      }),
    );
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      list: async () => [],
      sessions: async () => [],
    } as unknown as ManagerClient;
    const clock = new ManualClock(100);
    const tool = createTaskListTool({
      getClient: () => client,
      getIndex: () => new WorkIndex({ clock }),
      home,
      sessionId: () => "sess-mine",
      clock,
    });
    const res =
      await tool.execute("t", { all: true }, undefined as never, undefined as never, {} as never);
    const text = (res.content[0] as { text: string }).text;
    expect(text).toMatch(/ch_hosted_live \[agent\] running/);
    expect(text).not.toMatch(/ch_hosted_live \[agent\] interrupted/);
  });

  it("describes `all` as finished work, not other sessions", () => {
    const tool = createTaskListTool({ getClient: () => null });
    const schema = JSON.stringify(tool.parameters);
    expect(schema).not.toMatch(/all sessions/i);
    expect(schema).toMatch(/finished/i);
  });
});

describe("task_list monitor timeout and last output", () => {
  function fakeClient(): ManagerClient {
    return {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      list: async () => [],
    } as unknown as ManagerClient;
  }

  async function listText(opts: {
    now: number;
    items: Parameters<WorkIndex["upsert"]>[0][];
    live?: { taskId: string; remainingMs: number | null; persistent: boolean; lastOutputAt?: number }[];
  }): Promise<string> {
    const clock = new ManualClock(opts.now);
    const index = new WorkIndex({ clock });
    for (const item of opts.items) index.upsert(item);
    const tool = createTaskListTool({
      getClient: fakeClient,
      getIndex: () => index,
      clock,
      getMonitorLive: () => opts.live ?? [],
    });
    const res = await tool.execute("t", {}, undefined as never, undefined as never, {} as never);
    return (res.content[0] as { text: string }).text;
  }

  it("shows remaining timeout and last output on a timed monitor, not on shell or subagent rows", async () => {
    const text = await listText({
      now: 10_000,
      items: [
        { id: "mon_1", kind: "monitor", status: "running", title: "watch logs", startedAt: 0, countsAsWorker: false },
        { id: "sh_1", kind: "shell", status: "running", title: "npm test", startedAt: 0, countsAsWorker: true },
        { id: "ch_1", kind: "agent", status: "running", title: "review", startedAt: 0, countsAsWorker: false },
      ],
      live: [{ taskId: "mon_1", remainingMs: 90_000, persistent: false, lastOutputAt: 7_000 }],
    });
    expect(text).toMatch(/mon_1 \[monitor\] running \(10s\) "watch logs".*timeout in 1m30s/);
    expect(text).toMatch(/mon_1 \[monitor\].*last output 3s ago/);
    expect(text).toMatch(/sh_1 \[shell\] running \(10s\) "npm test"/);
    expect(text).not.toMatch(/sh_1.*timeout|sh_1.*last output|sh_1.*persistent/);
    expect(text).toMatch(/ch_1 \[agent\] running \(10s\) "review"/);
    expect(text).not.toMatch(/ch_1.*timeout|ch_1.*last output|ch_1.*persistent/);
  });

  it("shows persistent and no output yet for a monitor that has not printed", async () => {
    const text = await listText({
      now: 5_000,
      items: [
        { id: "mon_p", kind: "monitor", status: "running", title: "forever", startedAt: 0, countsAsWorker: false },
      ],
      live: [{ taskId: "mon_p", remainingMs: null, persistent: true }],
    });
    expect(text).toMatch(/mon_p \[monitor\] running \(5s\) "forever".*persistent/);
    expect(text).toMatch(/mon_p \[monitor\].*no output yet/);
    expect(text).not.toMatch(/timeout in/);
  });

  it("treats repeated deduped output as recent and omits last output when none arrived", async () => {
    const recent = await listText({
      now: 10_000,
      items: [
        { id: "mon_1", kind: "monitor", status: "running", title: "watch", startedAt: 0, countsAsWorker: false },
      ],
      live: [{ taskId: "mon_1", remainingMs: null, persistent: true, lastOutputAt: 9_000 }],
    });
    expect(recent).toMatch(/mon_1 \[monitor\].*last output 1s ago/);
    const none = await listText({
      now: 10_000,
      items: [
        { id: "mon_1", kind: "monitor", status: "running", title: "watch", startedAt: 0, countsAsWorker: false },
      ],
      live: [{ taskId: "mon_1", remainingMs: null, persistent: true }],
    });
    expect(none).toMatch(/mon_1 \[monitor\].*no output yet/);
    expect(none).not.toMatch(/last output/);
  });
});
