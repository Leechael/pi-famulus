/**
 * A hosted child's shell is a daemon task (origin child-bash) that
 * the child waits on in the foreground. When the child process dies, that
 * shell must not keep running unseen: it is stopped, or the parent is told
 * that the child left it behind.
 *
 * Real daemon, real `pi --mode rpc` child on the fake model, whose first
 * call runs bash on a command that never exits. The child is SIGKILLed
 * while it waits.
 *
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/hosted-child-orphan-shell.test.ts
 */
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { alive, HOSTED_INTEG, startHostedHarness, waitUntil, type HostedHarness } from "./hosted-harness";

interface TaskRecord {
  task_id: string;
  command: string;
  status: string;
  pid: number;
  origin?: { via?: string; child_id?: string };
}

function childShells(h: HostedHarness): TaskRecord[] {
  const dir = join(h.home, "sessions", h.sessionId, "tasks");
  if (!existsSync(dir)) return [];
  return readdirSync(dir)
    .filter((file) => file.endsWith(".json"))
    .map((file) => JSON.parse(readFileSync(join(dir, file), "utf8")) as TaskRecord)
    .filter((task) => task.origin?.via === "child-bash");
}

/** Processes whose command line names this hosted child's pi session. */
function childPids(childId: string): number[] {
  return execFileSync("ps", ["-eo", "pid=,args="], { encoding: "utf8" })
    .split("\n")
    .filter((line) => line.includes(`--session-id ${childId}`))
    .map((line) => Number(line.trim().split(/\s+/)[0]))
    .filter((pid) => pid > 0 && pid !== process.pid);
}

describe.skipIf(!HOSTED_INTEG)("a hosted child killed while its shell runs", () => {
  let harness: HostedHarness | null = null;

  afterEach(async () => {
    await harness?.cleanup();
    harness = null;
  });

  it("does not leave the child's shell running unseen", async () => {
    harness = await startHostedHarness({ scenario: "bash-hang" });
    const h = harness;
    await h.tool("subagent").execute("sub-1", { async: true, tasks: [{ prompt: "work", name: "worker" }] }, undefined, undefined, h.ctx);
    await waitUntil(() => childShells(h).some((task) => task.status === "running"), 30_000, "the child's shell to start");
    const shell = childShells(h)[0];
    const childId = String(shell.origin?.child_id);
    const pids = childPids(childId);
    expect(pids.length).toBeGreaterThan(0);

    for (const pid of pids) process.kill(pid, "SIGKILL");
    await waitUntil(() => pids.every((pid) => !alive(pid)), 5_000, "the child process to exit");
    await waitUntil(
      () => h.agentRecords().some((rec) => rec.child_id === childId && rec.status !== "running"),
      15_000,
      "the parent to settle the dead child",
    );
    const stillRunningNow = () => {
      const now = childShells(h).find((task) => task.task_id === shell.task_id);
      return now?.status === "running" && alive(shell.pid);
    };
    const toldNow = () => h.sent.some((sent) => (sent.message.content ?? "").includes(shell.task_id));
    // Room for a stop (SIGTERM, 2s grace, SIGKILL) or a notice to arrive; the
    // assertion below reports what was seen if neither did.
    await waitUntil(() => !stillRunningNow() || toldNow(), 15_000, "the shell to be stopped or reported").catch(() => {});

    const stillRunning = stillRunningNow();
    const told = toldNow();
    // Either outcome is correct: stopped, or left running with the parent told.
    expect({ stillRunning, told, handled: !stillRunning || told }).toEqual({ stillRunning, told, handled: true });
  }, 90_000);
});
