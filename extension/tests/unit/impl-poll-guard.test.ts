import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { loadConfig } from "../../src/config";
import { installPollGuard } from "../../src/poll-guard";
import { fill } from "../../src/prompts.generated";

// 2026-11-07 log-monitor session (deepseek-flash): 258 task_output calls, 326
// "no new output" replies. 2026-11-08 agent-wait-ready / monitor-not-sleep
// probes (gpt-6-luna): task_list/task_output while a monitor ran, before its event.

type Handler = (event: any) => unknown;
function setup(items: { id: string; status: string }[], enabled = true) {
  const handlers = new Map<string, Handler[]>();
  const blocks: unknown[] = [];
  installPollGuard({ on: (n: string, h: Handler) => handlers.set(n, [...(handlers.get(n) ?? []), h]) } as never, {
    enabled,
    getIndex: () => ({ list: () => items }),
    onBlock: (info) => blocks.push(info),
  });
  const emit = async (name: string, event: unknown) => {
    let out: unknown;
    for (const h of handlers.get(name) ?? []) out = (await h(event)) ?? out;
    return out as { block?: boolean; terminate?: boolean; reason?: string } | undefined;
  };
  const call = (toolName: string, input: Record<string, unknown> = {}) => emit("tool_call", { toolName, input, toolCallId: "c" });
  const wake = (kind: string, details: Record<string, unknown>) =>
    emit("message_end", { message: { role: "custom", customType: "pi-famulus-wake", content: "wake", details: { kind, ...details } } });
  const user = () => emit("message_end", { message: { role: "user", content: "status?" } });
  const started = (toolName: string, text: string) => emit("message_end", { message: { role: "toolResult", toolName, content: [{ type: "text", text }] } });
  return { call, wake, user, started, blocks, items };
}

describe("installPollGuard", () => {
  it("refuses task_output on a running task with nothing new, without ending the run", async () => {
    const g = setup([{ id: "sh_1", status: "running" }]);
    const r = await g.call("task_output", { task_id: "sh_1" });
    expect(r?.block).toBe(true);
    expect(r?.terminate).toBeUndefined();
    expect(r?.reason).toContain("sh_1 is still running");
    expect(r?.reason).toContain("start a monitor");
    expect(g.blocks).toEqual([{ tool: "task_output", taskId: "sh_1" }]);
  });

  it("allows one read of a task after its wake, then refuses the next", async () => {
    const g = setup([{ id: "mon_1", status: "running" }]);
    await g.wake("monitor", { id: "mon_1" });
    expect(await g.call("task_output", { task_id: "mon_1" })).toBeUndefined();
    expect((await g.call("task_output", { task_id: "mon_1" }))?.block).toBe(true);
  });

  it("allows paging (a cursor past 0), finished tasks and unknown ids", async () => {
    const g = setup([{ id: "sh_1", status: "running" }, { id: "sh_2", status: "completed" }]);
    expect(await g.call("task_output", { task_id: "sh_1", cursor: 4096 })).toBeUndefined();
    expect(await g.call("task_output", { task_id: "sh_1", cursor: 4096, max_bytes: 8192 })).toBeUndefined();
    expect(await g.call("task_output", { task_id: "sh_2" })).toBeUndefined();
    expect(await g.call("task_output", { task_id: "ch_9" })).toBeUndefined();
  });

  // 2026-11-08 E arm, gpt-6-luna: polls carried max_bytes, and task_list followed the starts in the same turn.
  it("refuses a fresh read that only sets max_bytes or cursor 0", async () => {
    const g = setup([{ id: "sh_e473e089", status: "running" }, { id: "mon_7613e62e", status: "running" }]);
    expect((await g.call("task_output", { task_id: "sh_e473e089", max_bytes: 1000 }))?.block).toBe(true);
    expect((await g.call("task_output", { task_id: "mon_7613e62e", max_bytes: 2000 }))?.block).toBe(true);
    expect((await g.call("task_output", { task_id: "mon_7613e62e", cursor: 0 }))?.block).toBe(true);
  });

  it("refuses task_list right after starting work, until a wake or message arrives", async () => {
    const g = setup([{ id: "sh_1", status: "running" }, { id: "mon_1", status: "running" }]);
    await g.user();
    await g.started("bash", 'Command "node server.cjs" moved to background (task_id: sh_1). Output: /tmp/x');
    await g.started("monitor", "Monitor started · task mon_1 · persistent");
    expect((await g.call("task_list", { all: true }))?.block).toBe(true);
    await g.wake("monitor", { id: "mon_1" });
    expect(await g.call("task_list", { all: true })).toBeUndefined();
  });

  it("recognises the real backgrounded-bash result text as a start", async () => {
    const g = setup([{ id: "sh_1", status: "running" }]);
    await g.started("bash", fill("tools.bash.result.backgrounded", { command: "node server.cjs", taskId: "sh_1", outputPath: "/tmp/o" }));
    expect((await g.call("task_list"))?.block).toBe(true);
  });

  it("allows the first task_list, refuses a repeat while work runs, and reopens after a wake or a user message", async () => {
    const g = setup([{ id: "mon_1", status: "running" }]);
    expect(await g.call("task_list")).toBeUndefined();
    const again = await g.call("task_list");
    expect(again?.block).toBe(true);
    expect(again?.reason).toContain("1 background item(s) are still running");
    await g.wake("task", { tasks: [{ id: "sh_9" }] });
    expect(await g.call("task_list")).toBeUndefined();
    expect((await g.call("task_list"))?.block).toBe(true);
    await g.user();
    expect(await g.call("task_list")).toBeUndefined();
  });

  it("does nothing when no work is active, for other tools, or when disabled", async () => {
    const idle = setup([{ id: "sh_1", status: "completed" }]);
    expect(await idle.call("task_list")).toBeUndefined();
    expect(await idle.call("task_list")).toBeUndefined();
    expect(await idle.call("task_output", { task_id: "sh_1" })).toBeUndefined();
    const busy = setup([{ id: "sh_1", status: "running" }]);
    expect(await busy.call("bash", { command: "ls" })).toBeUndefined();
    expect(await setup([{ id: "sh_1", status: "running" }], false).call("task_output", { task_id: "sh_1" })).toBeUndefined();
  });

  it("is on by default in config and can be turned off", () => {
    const home = mkdtempSync(join(tmpdir(), "pf-poll-guard-"));
    try {
      expect(loadConfig(home).pollGuard).toBe(true);
      writeFileSync(join(home, "config.json"), JSON.stringify({ pollGuard: false }));
      expect(loadConfig(home).pollGuard).toBe(false);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });
});
