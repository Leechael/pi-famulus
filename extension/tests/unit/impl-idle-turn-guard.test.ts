import { describe, expect, it } from "vitest";
import { IDLE_RUN_LIMIT, IDLE_TURN_LIMIT, installIdleTurnGuard, isIdleProbe } from "../../src/idle-turn-guard";

// Incidents 2026-11-07, both delegated over cross-session messages with no
// user turn:
// - incident A (k3.1-pro:high): after reporting back it ran 345 more
//   turns in 31 minutes, each "完成。" plus `task_list`, `date`, `true` or
//   `printf ''`, until the user aborted. Its thinking said "Stop." every turn.
// - incident B (k3-256k): waiting on a CI monitor it alternated `true`
//   and `task_list` for 8 minutes until the monitor's wake arrived.
// The prompt already says "If no actionable work remains, end your response
// without a tool call"; nothing in the harness enforced it.

type Handler = (event: any, ctx?: any) => unknown;

function setup(enabled = true) {
  const handlers = new Map<string, Handler[]>();
  const pi = {
    on: (name: string, h: Handler) => {
      handlers.set(name, [...(handlers.get(name) ?? []), h]);
    },
  };
  const fired: { count: number; calls: string[] }[] = [];
  installIdleTurnGuard(pi as never, { enabled, onTrip: (info) => fired.push(info) });
  let n = 0;
  const emit = async (name: string, event: unknown) => {
    let out: unknown;
    for (const h of handlers.get(name) ?? []) out = (await h(event)) ?? out;
    return out;
  };
  /** One assistant turn: message_end, then tool_call per call. Returns each call's hook result. */
  const turn = async (...calls: { name: string; args: Record<string, unknown> }[]) => {
    const content = calls.map((c) => ({ type: "toolCall", id: `c${++n}`, name: c.name, arguments: c.args }));
    await emit("message_end", { message: { role: "assistant", content: [{ type: "text", text: "完成。" }, ...content] } });
    const results = [];
    for (const c of content) results.push(await emit("tool_call", { toolCallId: c.id, toolName: c.name, input: c.arguments }));
    return results as ({ block?: boolean; terminate?: boolean; reason?: string } | undefined)[];
  };
  const reply = () => emit("message_end", { message: { role: "assistant", content: [{ type: "text", text: "done" }] } });
  const wake = () => emit("message_end", { message: { role: "custom", customType: "pi-famulus-wake", content: "wake" } });
  return { turn, reply, wake, handlers, fired };
}

const bash = (command: string) => ({ name: "bash", args: { command, timeout: 30 } });
const taskList = (args: Record<string, unknown> = {}) => ({ name: "task_list", args });

describe("isIdleProbe", () => {
  it.each([
    "true",
    ":",
    "date -u +'%Y-%m-%d %H:%M:%S UTC'",
    "date -u +'%H:%M:%S'",
    "printf ''",
    "printf 'done\\n'",
    "echo done",
    "sleep 30",
    "exit 0",
  ])("bash %j does nothing", (command) => {
    expect(isIdleProbe("bash", { command })).toBe(true);
  });

  it.each([
    "git status --short",
    "echo x > result.txt",
    "date; rm -rf build",
    "true && make",
    "gh pr checks 101",
    "printf 'x' | tee out",
    "echo $(make)",
  ])("bash %j does work", (command) => {
    expect(isIdleProbe("bash", { command })).toBe(false);
  });

  it("task_list is a status probe; task_output and edits are not", () => {
    expect(isIdleProbe("task_list", {})).toBe(true);
    expect(isIdleProbe("task_list", { all: true })).toBe(true);
    expect(isIdleProbe("task_output", { task_id: "sh_1" })).toBe(false);
    expect(isIdleProbe("edit", { path: "a" })).toBe(false);
  });
});

describe("idle turn guard", () => {
  it("lets a single status check through", async () => {
    const { turn } = setup();
    expect(await turn(taskList())).toEqual([undefined]);
  });

  it(`ends the run on the ${IDLE_TURN_LIMIT}nd consecutive idle turn (incident A replay)`, async () => {
    const { turn, fired } = setup();
    expect(await turn(bash("git -C wt status --short --branch"))).toEqual([undefined]);
    expect(await turn(taskList())).toEqual([undefined]);
    const [r] = await turn(bash("date -u +'%Y-%m-%d %H:%M:%S UTC'"));
    expect(r).toMatchObject({ block: true, terminate: true });
    expect(r!.reason).toMatch(/no tool call/);
    expect(r!.reason).toMatch(/2 turns in a row/);
    expect(fired).toEqual([{ count: 2, calls: ["task_list", "bash: date -u +'%Y-%m-%d %H:%M:%S UTC'"] }]);
  });

  it("ends the run while waiting on a monitor (incident B replay)", async () => {
    const { turn } = setup();
    expect(await turn(bash("true"))).toEqual([undefined]);
    const [r] = await turn(taskList());
    expect(r).toMatchObject({ block: true, terminate: true });
  });

  it("blocks every call of an all-idle batch so pi can end the run", async () => {
    const { turn } = setup();
    await turn(bash("true"));
    const rs = await turn(taskList(), bash("date"));
    expect(rs.every((r) => r?.block === true && r?.terminate === true)).toBe(true);
  });

  it("a batch with real work is not idle and breaks the streak", async () => {
    const { turn } = setup();
    await turn(bash("true"));
    expect(await turn(bash("true"), { name: "edit", args: { path: "a" } })).toEqual([undefined, undefined]);
    expect(await turn(taskList())).toEqual([undefined]);
  });

  // Replay of incident A from its final report (k3.1-pro:high, guard off): the
  // model interleaved no-ops with fresh read-only checks, so no two idle turns
  // were adjacent until turn 6.
  it(`ends the run at ${IDLE_RUN_LIMIT} idle turns in one run, even interleaved (replay)`, async () => {
    const { turn } = setup();
    expect(await turn(bash("git -C wt status --short --branch"))).toEqual([undefined]);
    expect(await turn(taskList())).toEqual([undefined]);
    expect(await turn(bash("gh pr view 101 --json isDraft"))).toEqual([undefined]);
    expect(await turn(bash("pwd"))).toEqual([undefined]);
    expect(await turn({ name: "read", args: { path: "docker-compose.yml" } })).toEqual([undefined]);
    const [r] = await turn(bash("echo done"));
    expect(r).toMatchObject({ block: true, terminate: true });
    expect(r!.reason).toMatch(/3 turns in this run/);
  });

  it("real work between idle turns still counts toward the run limit, but a reply resets it", async () => {
    const { turn, reply } = setup();
    await turn(taskList());
    await turn(bash("make"));
    await turn(taskList());
    await reply();
    await turn(bash("make"));
    expect(await turn(taskList())).toEqual([undefined]);
  });

  it("a reply without tool calls resets the streak", async () => {
    const { turn, reply } = setup();
    await turn(bash("true"));
    await reply();
    expect(await turn(taskList())).toEqual([undefined]);
  });

  it("a wake or user message resets the streak: it is new information", async () => {
    const { turn, wake } = setup();
    await turn(bash("true"));
    await wake();
    expect(await turn(taskList())).toEqual([undefined]);
  });

  it("a new run starts with a clean streak", async () => {
    const { turn, handlers } = setup();
    await turn(bash("true"));
    await turn(bash("true"));
    for (const h of handlers.get("agent_start") ?? []) await h({ type: "agent_start" });
    expect(await turn(taskList())).toEqual([undefined]);
  });

  it("keeps blocking if the model idles again after a block (steered continuation)", async () => {
    const { turn } = setup();
    await turn(bash("true"));
    await turn(bash("true"));
    const [r] = await turn(bash("date"));
    expect(r).toMatchObject({ block: true, terminate: true });
    expect(r!.reason).toMatch(/3 turns in a row/);
  });

  it("ignores calls a tool made (codemode nested calls)", async () => {
    const { turn, handlers } = setup();
    await turn(bash("true"));
    await turn(bash("true"));
    const h = handlers.get("tool_call")![0];
    expect(await h({ toolCallId: "c2/1", parentToolCallId: "c2", toolName: "bash", input: { command: "true" } })).toBeUndefined();
  });

  it("is off when disabled in config", async () => {
    const { handlers } = setup(false);
    expect(handlers.size).toBe(0);
  });
});

describe("idleTurnGuard config", () => {
  it("defaults on and can be turned off in config.json", async () => {
    const { mkdtempSync, writeFileSync } = await import("node:fs");
    const { tmpdir } = await import("node:os");
    const { join } = await import("node:path");
    const { loadConfig } = await import("../../src/config");
    const home = mkdtempSync(join(tmpdir(), "famulus-guard-"));
    expect(loadConfig(home).idleTurnGuard).not.toBe(false);
    writeFileSync(join(home, "config.json"), JSON.stringify({ idleTurnGuard: false }));
    expect(loadConfig(home).idleTurnGuard).toBe(false);
  });
});
