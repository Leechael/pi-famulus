/**
 * Eval batch 4 (2026-09-30): gpt-6-luna ran `./gen.sh` with `timeout: 1`. The
 * manager killed it after 1s and bash returned "generating..." as a success,
 * so the model believed it was still running. The local fallback already
 * reported "Command timed out"; the manager path treated a signal kill
 * (exit_code null) as exit 0.
 */
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { createBashOverride } from "../../src/bash-override";
import { DEFAULT_CONFIG } from "../../src/config";
import type { ManagerClient, TaskRecord } from "../../src/manager-client";

function run(record: Partial<TaskRecord>, params: { command: string; timeout?: number }, outputStatus = "killed") {
  const client = {
    ensureAvailable: vi.fn(async () => true),
    start: vi.fn(async () => ({ task_id: "sh_1", pid: 1 })),
    wait: vi.fn(async () => ({ done: true, exit_code: null })),
    output: vi.fn(async () => ({ chunk: "generating...\n", next_cursor: 14, status: outputStatus, exit_code: null, total_size: 14 })),
    list: vi.fn(async () => [{ task_id: "sh_1", status: "killed", exit_code: null, ...record }]),
    stop: vi.fn(async () => {}),
  } as unknown as ManagerClient;
  const tool = createBashOverride({
    getClient: () => client,
    config: { ...DEFAULT_CONFIG, foregroundBudgetMs: 50 },
    home: "/tmp/pi-famulus-test",
    sessionId: () => "s",
    sessionEnv: () => ({}),
    trackTask: vi.fn(),
    markNotifyOnExit: vi.fn(),
  });
  const ctx = { cwd: "/tmp", sessionManager: { getSessionId: () => "s", getSessionFile: () => null } } as unknown as ExtensionToolContext;
  return tool.execute("tc", params, undefined, undefined, ctx);
}

describe("bash on the manager path, command killed", () => {
  it("reports a timeout kill as a timeout, keeping the output", async () => {
    await expect(run({ end_reason: "timeout", signal: "SIGTERM" }, { command: "./gen.sh", timeout: 1 })).rejects.toThrow(
      /generating\.\.\.[\s\S]*timed out after 1 second/,
    );
  });

  // cubic review on #19: the manager records a command whose runner status
  // was unobservable as completed with a null exit code. That is not a kill.
  it("returns a completed task with no exit code as a success", async () => {
    const res = await run({ status: "completed" }, { command: "./gen.sh" }, "completed");
    expect((res.content[0] as { text: string }).text).toContain("generating...");
  });

  it("reports any other kill as killed, with the signal", async () => {
    await expect(run({ signal: "SIGKILL" }, { command: "./gen.sh" })).rejects.toThrow(/killed \(SIGKILL\)/);
  });
});

// cubic review on #19: the local fallback (manager unavailable) had the same
// gap: a command killed by a signal came back as a success.
describe("bash on the local fallback", () => {
  function localTool() {
    return createBashOverride({
      getClient: () => null,
      config: DEFAULT_CONFIG,
      home: join(tmpdir(), "pi-famulus-test"),
      sessionId: () => "s",
      sessionEnv: () => ({}),
      trackTask: vi.fn(),
      markNotifyOnExit: vi.fn(),
    });
  }
  const ctx = { cwd: tmpdir(), sessionManager: { getSessionId: () => "s", getSessionFile: () => null } } as unknown as ExtensionToolContext;

  // Windows has no signal kills to report.
  it.skipIf(process.platform === "win32")("reports a signal kill as killed", async () => {
    await expect(localTool().execute("tc", { command: "echo partial; kill -9 $$" }, undefined, undefined, ctx)).rejects.toThrow(
      /partial[\s\S]*killed \(SIGKILL\)/,
    );
  });

  it("runs the command in pi's POSIX shell", async () => {
    const res = await localTool().execute("tc", { command: "x=21; echo \"answer=$((x * 2))\"" }, undefined, undefined, ctx);
    expect((res.content[0] as { text: string }).text.trim()).toBe("answer=42");
  });
});
