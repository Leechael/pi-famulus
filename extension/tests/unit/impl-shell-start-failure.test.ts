/**
 * Windows Git Bash start-up race (git-for-windows/git#6368): the manager ends
 * a task whose shell crashed while starting as shell-start-failed, and one
 * whose shell hung while starting as shell-start-hung. Every place the model
 * hears about the task must say the command did not run, so it retries
 * instead of debugging "exit code -1073741819" (PR #75 CI, win32-arm64).
 */
import { describe, expect, it, vi } from "vitest";
import type { ExtensionContext, ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { createBashOverride } from "../../src/bash-override";
import { ManualClock } from "../../src/clock";
import { DEFAULT_CONFIG } from "../../src/config";
import { formatTaskNotification, type TaskExitInfo } from "../../src/format";
import type { ManagerClient, TaskRecord } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import { exitedStatus, killedStatus } from "../../src/shell-exec";
import { createChildBashTool } from "../../src/subagent/child-bash";

const CRASH = -1073741819;

function client(record: Partial<TaskRecord>, exitCode: number | null): ManagerClient {
  const status = exitCode === null ? "killed" : "failed";
  return {
    ensureAvailable: vi.fn(async () => true),
    start: vi.fn(async () => ({ task_id: "sh_1", pid: 1 })),
    wait: vi.fn(async () => ({ done: true, exit_code: exitCode })),
    output: vi.fn(async () => ({ chunk: "", next_cursor: 0, status, exit_code: exitCode, total_size: 0 })),
    list: vi.fn(async () => [{ task_id: "sh_1", status, exit_code: exitCode, ...record }]),
    stop: vi.fn(async () => {}),
  } as unknown as ManagerClient;
}

describe("status lines", () => {
  it("a start-up crash on Windows says the shell failed to start", async () => {
    const text = await exitedStatus(client({ end_reason: "shell-start-failed" }, CRASH), "sh_1", CRASH, "win32");
    expect(text).toMatch(/Git Bash failed to start \(Windows runtime race, not your command\).*Retry/);
    expect(text).toContain(`exit code ${CRASH}`);
  });

  it("an ordinary non-zero exit stays as it was, and skips the lookup off Windows", async () => {
    const c = client({ end_reason: "exited" }, 2);
    expect(await exitedStatus(c, "sh_1", 2, "win32")).toBe("Command exited with code 2");
    expect(await exitedStatus(c, "sh_1", 2, "linux")).toBe("Command exited with code 2");
    expect(c.list).toHaveBeenCalledTimes(1);
  });

  it("a hung start-up kill says the shell hung, not 'killed'", async () => {
    const text = await killedStatus(client({ end_reason: "shell-start-hung" }, null), "sh_1", undefined);
    expect(text).toMatch(/Git Bash hung while starting \(Windows runtime race, not your command\).*Retry/);
  });
});

describe("bash tools", () => {
  const ctx = { cwd: "/tmp", sessionManager: { getSessionId: () => "s", getSessionFile: () => null } } as unknown as ExtensionToolContext;

  it("main-agent bash reports a hung start-up as an error", async () => {
    const tool = createBashOverride({
      getClient: () => client({ end_reason: "shell-start-hung" }, null),
      config: { ...DEFAULT_CONFIG, foregroundBudgetMs: 50 },
      home: "/tmp/pi-famulus-test",
      sessionId: () => "s",
      sessionEnv: () => ({}),
      trackTask: vi.fn(),
      markNotifyOnExit: vi.fn(),
    });
    await expect(tool.execute("tc", { command: "make" }, undefined, undefined, ctx)).rejects.toThrow(/Git Bash hung while starting/);
  });

  // A subagent's bash must fail too, so the subagent moves on and its slots
  // are freed instead of waiting on a shell that never starts.
  it("subagent bash fails with the same message", async () => {
    const tool = createChildBashTool({
      getClient: () => client({ end_reason: "shell-start-hung" }, null),
      home: "/tmp/pi-famulus-test",
      sessionId: () => "s",
      sessionEnv: () => ({}),
      trackTask: vi.fn(),
      childId: "ch_1",
      runId: "run_1",
    });
    await expect(tool.execute("tc", { command: "make" }, undefined, undefined, ctx)).rejects.toThrow(/Git Bash hung while starting/);
  });
});

describe("wakes", () => {
  it("a background task's exit wake says the command did not run", () => {
    const info: TaskExitInfo = {
      taskId: "sh_1",
      kind: "shell",
      command: "npm test",
      status: "failed",
      exitCode: CRASH,
      durationMs: 15_800,
      outputPath: "/tmp/sh_1.output",
      preview: "",
      endReason: "shell-start-failed",
    };
    const xml = formatTaskNotification([info]).content;
    expect(xml).toMatch(/did not run\. Git Bash failed to start \(Windows runtime race, not your command\)/);
  });

  it("a monitor's exit notice says Git Bash failed to start", async () => {
    const clock = new ManualClock();
    const sent: { content: string }[] = [];
    const manager = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      start: async () => ({ task_id: "mon_1", pid: 1 }),
      watch: async () => {},
      stop: async () => {},
    } as unknown as ManagerClient;
    const center = new NotifyCenter({ sendMessage: (m) => sent.push(m as { content: string }), isIdle: () => true, clock });
    const registry = new MonitorRegistry({
      getClient: () => manager,
      sessionEnv: () => ({}),
      getNotifyCenter: () => center,
      trackTask: () => {},
      clock,
    });
    await registry.start({ command: "tail -F log", description: "log" }, { cwd: "/tmp" } as ExtensionContext);
    registry.handleExit("mon_1", {
      event: "task_exited",
      task_id: "mon_1",
      exit_code: CRASH,
      duration_ms: 15_800,
      end_reason: "shell-start-failed",
    });
    center.settled();
    expect(sent.map((m) => m.content).join("\n")).toMatch(/Git Bash failed to start \(Windows runtime race, not your command\)/);
  });
});
