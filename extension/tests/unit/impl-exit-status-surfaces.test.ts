/**
 * One exit must be the same status on every surface that reports it.
 *
 * The daemon record comes from terminal_status (manager/src/registry.rs).
 * Foreground bash reads that record (bash-override.ts, the null-exit branch).
 * The background wake maps the task_exited event through toExitStatus
 * (index.ts). task_exited carries exit_code and signal, not the record status
 * (manager/src/proto.rs TaskExited).
 */
import { describe, expect, it, vi } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { createBashOverride } from "../../src/bash-override";
import { DEFAULT_CONFIG } from "../../src/config";
import { toExitStatus } from "../../src/index";
import type { ManagerClient, ManagerEvent, TaskRecord } from "../../src/manager-client";

type SurfaceStatus = "completed" | "failed" | "killed" | "orphaned";

interface ExitCase {
  name: string;
  /** Serialized TaskStatus from terminal_status for this observation. */
  recordStatus: SurfaceStatus;
  exitCode: number | null;
  signal: string | null;
  endReason: string;
}

const CASES: ExitCase[] = [
  {
    name: "normal exit 0",
    // terminal_status(false, Some(0), None) == Completed
    recordStatus: "completed",
    exitCode: 0,
    signal: null,
    endReason: "exited",
  },
  {
    name: "nonzero exit",
    // terminal_status(false, Some(1), None) == Failed
    recordStatus: "failed",
    exitCode: 1,
    signal: null,
    endReason: "exited",
  },
  {
    name: "killed by an external signal",
    // terminal_status(false, None, Some(15)) == Failed. The event names the signal.
    recordStatus: "failed",
    exitCode: null,
    signal: "SIGTERM",
    endReason: "exited",
  },
  {
    name: "unreadable status",
    // terminal_status(false, None, None) == Completed. task_exited has no status.
    recordStatus: "completed",
    exitCode: null,
    signal: null,
    endReason: "exited",
  },
];

function wakeStatus(row: ExitCase): SurfaceStatus {
  const event: ManagerEvent = {
    event: "task_exited",
    task_id: "sh_1",
    exit_code: row.exitCode,
    signal: row.signal,
    end_reason: row.endReason,
    duration_ms: 10,
    output_path: "/tmp/sh_1.output",
  };
  return toExitStatus(event);
}

async function foregroundStatus(row: ExitCase): Promise<SurfaceStatus> {
  const record = {
    task_id: "sh_1",
    status: row.recordStatus,
    exit_code: row.exitCode,
    signal: row.signal,
    end_reason: row.endReason,
  } as TaskRecord;
  const client = {
    ensureAvailable: vi.fn(async () => true),
    start: vi.fn(async () => ({ task_id: "sh_1", pid: 1 })),
    wait: vi.fn(async () => ({ done: true, exit_code: row.exitCode })),
    output: vi.fn(async () => ({
      chunk: "out\n",
      next_cursor: 4,
      status: row.recordStatus,
      exit_code: row.exitCode,
      total_size: 4,
    })),
    list: vi.fn(async () => [record]),
    stop: vi.fn(async () => {}),
  } as unknown as ManagerClient;
  const tool = createBashOverride({
    getClient: () => client,
    config: { ...DEFAULT_CONFIG, foregroundBudgetMs: 20_000 },
    home: "/tmp/pi-famulus-test",
    sessionId: () => "s",
    sessionEnv: () => ({}),
    trackTask: vi.fn(),
    markNotifyOnExit: vi.fn(),
  });
  const ctx = {
    cwd: "/tmp",
    sessionManager: { getSessionId: () => "s", getSessionFile: () => null },
  } as unknown as ExtensionToolContext;
  try {
    const result = await tool.execute("tc", { command: "echo out" }, undefined, undefined, ctx);
    return result.isError ? "failed" : "completed";
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return /\bkilled\b/.test(message) ? "killed" : "failed";
  }
}

describe("exit status agrees across daemon record, foreground bash, and the background wake", () => {
  it.each(CASES)("$name", async (row) => {
    const record = row.recordStatus;
    const foreground = await foregroundStatus(row);
    const wake = wakeStatus(row);
    expect({ record, foreground, wake }).toEqual({ record, foreground: record, wake: record });
  });
});
