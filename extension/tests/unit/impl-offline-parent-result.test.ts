/**
 * A warm hosted child keeps the daemon alive after the parent dies.
 * A background bash this process did not start must be adopted on connect:
 * task_list lists it, and its exit arrives as a wake. Work that already
 * finished while the parent was gone is delivered the same way.
 *
 * Distinct from impl-session-restart-recovery, which covers
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
  settleAsync,
  startHostedExtension,
  textOf,
  until,
} from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

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

describe("results that finish while the parent is gone", () => {
  it("does not adopt a current foreground shell while bash is still waiting", async () => {
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    let complete!: () => void;
    const gate = new Promise<void>((resolve) => { complete = resolve; });
    try {
      await settleAsync();
      const marked = vi.spyOn(ext.manager, "markBackground");
      ext.manager.wait = async () => { await gate; return { done: true, exit_code: 0 }; };
      ext.manager.output = async () => ({ chunk: "foreground", next_cursor: 10, status: "completed", exit_code: 0, total_size: 10 });
      const pending = ext.pi.tool("bash").execute("fg", { command: "printf foreground" }, undefined, undefined, ext.ctx);
      await settleAsync();
      hostedFakeState().tasks = [shellTask({ task_id: "sh_late", origin: { via: "bash-fg" } })];
      await ext.pi.tool("task_list").execute("list", {}, undefined, undefined, ext.ctx);
      expect(marked).not.toHaveBeenCalled();
      complete();
      expect(textOf(await pending)).toBe("foreground");
      expect(ext.pi.sent).toHaveLength(0);
    } finally {
      complete();
      await ext.shutdown();
    }
  });

  it("does not redeliver a consumed foreground result as a background wake", async () => {
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      await settleAsync();
      ext.manager.wait = async () => ({ done: true, exit_code: 0 });
      ext.manager.output = async () => ({ chunk: "foreground", next_cursor: 10, status: "completed", exit_code: 0, total_size: 10 });
      const result = await ext.pi.tool("bash").execute("fg", { command: "printf foreground" }, undefined, undefined, ext.ctx);
      expect(textOf(result)).toBe("foreground");
      ext.manager.emit({ event: "task_exited", task_id: "sh_late", exit_code: 0, duration_ms: 10, output_path: join(home, "output") });
      hostedFakeState().tasks = [shellTask({
        task_id: "sh_late", origin: { via: "bash-fg" }, status: "completed", exit_code: 0, ended_at: Date.now(),
      })];
      await ext.pi.tool("task_list").execute("list", {}, undefined, undefined, ext.ctx);
      await new Promise((resolve) => setTimeout(resolve, 300));
      expect(ext.pi.sent).toHaveLength(0);
    } finally {
      await ext.shutdown();
    }
  });

  it.each(["bash-bg", "bash-fg"])("adopts an unknown %s shell and wakes when it exits", async (via) => {
    hostedFakeState().tasks = [shellTask({ origin: { via } })];
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

  it.each(["startup", "resume"])("delivers a parent shell that already finished while offline on %s", async (reason) => {
    hostedFakeState().tasks = [shellTask({
      status: "completed",
      exit_code: 0,
      ended_at: Date.now() - 500,
      output_size: 12,
    })];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, reason });
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

  it("a fresh reload runtime does not replay finished shells on connect or later task_list calls", async () => {
    hostedFakeState().tasks = ["bash-fg", "bash-bg"].map((via) => shellTask({
      task_id: `sh_finished_${via}`,
      origin: { via },
      status: "completed",
      exit_code: 0,
      ended_at: Date.now() - 500,
    }));
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, reason: "reload" });
    try {
      await settleAsync();
      for (let i = 0; i < 2; i++) {
        const listed = await ext.pi.tool("task_list").execute(`list-${i}`, { all: true }, undefined, undefined, ext.ctx);
        expect(textOf(listed)).toBe("No tasks.");
      }
      await new Promise((resolve) => setTimeout(resolve, 300));
      expect(ext.pi.sent).toHaveLength(0);
    } finally {
      await ext.shutdown();
    }
  });

  it("a fresh reload runtime adopts a running shell and reports its later exit only once", async () => {
    hostedFakeState().tasks = [shellTask({})];
    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home, reason: "reload" });
    try {
      await ext.pi.tool("task_list").execute("list-running", {}, undefined, undefined, ext.ctx);
      expect(ext.pi.sent).toHaveLength(0);
      const ended = shellTask({ status: "completed", exit_code: 0, ended_at: Date.now() });
      hostedFakeState().tasks = [ended];
      ext.manager.emit({ event: "task_exited", task_id: "sh_bg", kind: "shell", exit_code: 0 });
      await until(() => ext.pi.sent.length === 1, "the running shell's exit wake after reload");
      await ext.pi.tool("task_list").execute("list-ended", { all: true }, undefined, undefined, ext.ctx);
      ext.manager.emit({ event: "task_exited", task_id: "sh_bg", kind: "shell", exit_code: 0 });
      await new Promise((resolve) => setTimeout(resolve, 300));
      expect(ext.pi.sent).toHaveLength(1);
    } finally {
      await ext.shutdown();
    }
  });
});
