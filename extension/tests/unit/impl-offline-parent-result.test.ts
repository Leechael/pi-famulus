/**
 * I-20. A warm hosted child keeps the daemon alive after the parent dies.
 * A background bash this process did not start must be adopted on connect:
 * task_list lists it, and its exit arrives as a wake. Work that already
 * finished while the parent was gone is delivered the same way.
 *
 * Distinct from impl-session-restart-recovery (I-19/I-40), which covers
 * work the daemon already ended as manager-shutdown and stale running
 * agent records. The gated integration test hosted-offline-parent-result
 * covers the same path against a real daemon.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import {
  hostedFakeState,
  resetHostedFakes,
  startHostedExtension,
  textOf,
  until,
} from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-offline-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

const SESSION = "offline-sess";

function shellTask(partial: Record<string, unknown>): Record<string, unknown> {
  return {
    task_id: "sh_bg",
    session_id: SESSION,
    kind: "shell",
    command: "sleep 6; echo BG-DONE",
    cwd: home,
    pid: 4242,
    status: "running",
    exit_code: null,
    signal: null,
    started_at: Date.now() - 2_000,
    ended_at: null,
    output_path: join(home, "sessions", SESSION, "tasks", "sh_bg.output"),
    output_size: 0,
    origin: { via: "bash-bg" },
    ...partial,
  };
}

describe("I-20 results that finish while the parent is gone", () => {
  it("lists a still-running parent shell and wakes when it exits", async () => {
    hostedFakeState().tasks = [shellTask({})];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      const listed = await ext.pi.tool("task_list").execute("list-1", { all: true }, undefined, undefined, ext.ctx);
      expect(textOf(listed)).toContain("sh_bg");
      ext.manager.emit({
        event: "task_exited",
        task_id: "sh_bg",
        kind: "shell",
        command: "sleep 6; echo BG-DONE",
        exit_code: 0,
        duration_ms: 6_000,
        output_path: join(home, "sessions", SESSION, "tasks", "sh_bg.output"),
        ts: Date.now(),
      });
      await until(() => {
        return ext.pi.sent.some((entry) => {
          const details = entry.message.details as { kind?: string; taskId?: string } | undefined;
          const content = entry.message.content ?? "";
          return details?.kind === "task" && (details.taskId === "sh_bg" || content.includes("sh_bg"));
        });
      }, "a task wake for the adopted command");
    } finally {
      await ext.shutdown();
    }
  });

  it("delivers a parent shell that already finished while offline", async () => {
    hostedFakeState().tasks = [shellTask({
      status: "completed",
      exit_code: 0,
      ended_at: Date.now() - 500,
      output_size: 12,
    })];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      const listed = await ext.pi.tool("task_list").execute("list-2", { all: true }, undefined, undefined, ext.ctx);
      expect(textOf(listed)).toContain("sh_bg");
      await until(() => {
        return ext.pi.sent.some((entry) => {
          const details = entry.message.details as { kind?: string; taskId?: string } | undefined;
          const content = entry.message.content ?? "";
          return details?.kind === "task" && (details.taskId === "sh_bg" || content.includes("sh_bg"));
        });
      }, "a task wake for the offline result");
    } finally {
      await ext.shutdown();
    }
  });
});
