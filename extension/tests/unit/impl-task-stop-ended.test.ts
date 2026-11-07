import { describe, expect, it } from "vitest";
import type { ManagerClient, StopResult } from "../../src/manager-client";
import { createTaskStopTool } from "../../src/task-tools";

// Incident 2026-11-07 (k3.1-pro): task_stop on two
// `gh pr checks` tasks that had exited 0 five minutes earlier answered
// "Task … stopped (SIGTERM; SIGKILL after 2s grace if needed)." The manager
// treated the stop as an idempotent no-op; the tool reported a kill anyway.
describe("task_stop on a task that already ended", () => {
  function setup(result: StopResult) {
    const order: string[] = [];
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      stop: async (id: string) => {
        order.push(`stop ${id}`);
        return result;
      },
    } as unknown as ManagerClient;
    const tool = createTaskStopTool({
      getClient: () => client,
      noteStopRequested: (id) => {
        order.push(`note ${id}`);
        return () => order.push(`undo ${id}`);
      },
    });
    const run = async (id = "sh_1") => {
      const res = await tool.execute("t", { task_id: id }, undefined as never, undefined as never, {} as never);
      return { text: (res.content[0] as { text: string }).text, details: res.details };
    };
    return { run, order };
  }

  it("says the task had already ended instead of claiming a SIGTERM", async () => {
    const { run } = setup({ alreadyEnded: { status: "completed", exitCode: 0, signal: null } });
    const { text, details } = await run();
    expect(text).not.toMatch(/SIGTERM|stopped \(/);
    expect(text).toMatch(/already ended/i);
    expect(text).toMatch(/completed/);
    expect(text).toMatch(/exit code 0/);
    expect(details).toMatchObject({ task_id: "sh_1", stopped: false });
  });

  it("names the signal of a task that died of one", async () => {
    const { run } = setup({ alreadyEnded: { status: "killed", exitCode: null, signal: "SIGTERM" } });
    const { text } = await run();
    expect(text).toMatch(/already ended/i);
    expect(text).toMatch(/killed by SIGTERM/);
  });

  it("rolls back the stop note: nothing was stopped", async () => {
    const { run, order } = setup({ alreadyEnded: { status: "failed", exitCode: 3, signal: null } });
    await run();
    expect(order).toEqual(["note sh_1", "stop sh_1", "undo sh_1"]);
  });

  it("still reports a real stop as one", async () => {
    const { run, order } = setup({ alreadyEnded: null });
    const { text, details } = await run();
    expect(text).toMatch(/stopped \(SIGTERM/);
    expect(details).toMatchObject({ stopped: true });
    expect(order).toEqual(["note sh_1", "stop sh_1"]);
  });
});

describe("ManagerClient.stop", () => {
  it("parses already_ended from the daemon response", async () => {
    const { ManagerClient } = await import("../../src/manager-client");
    const client = Object.create(ManagerClient.prototype) as ManagerClient;
    const fake = client as unknown as { request: (m: unknown) => Promise<unknown> };
    fake.request = async () => ({ ok: true, already_ended: { status: "completed", exit_code: 0 } });
    expect(await client.stop("sh_1")).toEqual({ alreadyEnded: { status: "completed", exitCode: 0, signal: null } });
    fake.request = async () => ({ ok: true });
    expect(await client.stop("sh_1")).toEqual({ alreadyEnded: null });
  });
});
