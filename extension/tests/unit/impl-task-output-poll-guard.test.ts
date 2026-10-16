import { describe, expect, it } from "vitest";
import type { ManagerClient, OutputResponse } from "../../src/manager-client";
import { BEHAVIOR_GUIDELINES } from "../../src/behavior-guidelines";
import { ManualClock } from "../../src/clock";
import { formatBackgroundNotice } from "../../src/format";
import { createTaskListTool, createTaskOutputTool } from "../../src/task-tools";
import { WorkIndex } from "../../src/work-index";

// Manual testing (2026-09-29): with a prek hook and `gh pr checks --watch`
// backgrounded, the agent called task_output dozens of times in one run, each
// returning "(no output yet)" while the task kept running. The prompt said
// "do not poll" in three places; the tool itself never pushed back.
describe("task_output poll guard", () => {
  function setup() {
    const state: OutputResponse = { chunk: "", next_cursor: 0, status: "running", exit_code: null, total_size: 0 };
    const client = {
      ensureAvailable: async () => true,
      isAvailable: () => true,
      output: async (_id: string, cursor: number, _max: number) => ({
        ...state,
        chunk: state.chunk.slice(cursor),
        next_cursor: state.total_size,
      }),
    } as unknown as ManagerClient;
    const tool = createTaskOutputTool({ getClient: () => client });
    const read = async (id = "sh_1", cursor?: number) => {
      const params = cursor === undefined ? { task_id: id } : { task_id: id, cursor };
      const res = await tool.execute("t", params, undefined as never, undefined as never, {} as never);
      return (res.content[0] as { text: string }).text;
    };
    const write = (text: string) => {
      state.chunk += text;
      state.total_size = state.chunk.length;
    };
    return { state, read, write };
  }

  it("tells the model to end its turn when a running task has nothing yet", async () => {
    const { read } = setup();
    const text = await read();
    expect(text).toMatch(/with no tool call/i);
    expect(text).toMatch(/pi-famulus-wake/);
  });

  it("refuses a repeat read of a running task with no new output", async () => {
    const { read } = setup();
    await read();
    await expect(read()).rejects.toThrow(/with no tool call/i);
  });

  it("allows another read once the task has written more", async () => {
    const { read, write } = setup();
    write("line 1\n");
    await read();
    write("line 2\n");
    expect(await read()).toContain("line 2");
    await expect(read()).rejects.toThrow(/no new output/i);
  });

  // Batch 5/6 transcripts: models poll a running task with `cursor: 0` (53
  // such reads, 29 refused by this guard, mostly grok-4.3). Exempting reads
  // of an earlier window, as a review suggested, let every one of them
  // through once the task had printed a line.
  it("refuses a repeat cursor-0 read of a running task with no new output", async () => {
    const { read, write } = setup();
    write("compiling...\n");
    await read("sh_1", 0);
    await expect(read("sh_1", 0)).rejects.toThrow(/no new output/i);
  });

  it("tracks tasks separately", async () => {
    const { read } = setup();
    await read("sh_1");
    await expect(read("sh_2")).resolves.toMatch(/with no tool call/i);
  });

  it("never blocks reads of a finished task", async () => {
    const { state, read, write } = setup();
    write("done\n");
    await read();
    state.status = "completed";
    state.exit_code = 0;
    expect(await read()).toContain("done");
    expect(await read()).toContain("done");
  });

  it("refuses a repeat read of a running subagent with nothing new", async () => {
    const index = new WorkIndex({ clock: new ManualClock(0) });
    index.upsert({ id: "ch_1", kind: "agent", status: "running", title: "c", startedAt: 0, countsAsWorker: false });
    const tool = createTaskOutputTool({ getClient: () => null, getIndex: () => index });
    const read = () => tool.execute("t", { task_id: "ch_1" }, undefined as never, undefined as never, {} as never);
    const first = await read();
    expect((first.content[0] as { text: string }).text).toMatch(/with no tool call/i);
    await expect(read()).rejects.toThrow(/no new output/i);
  });

  it("allows a same-length rewrite of a running subagent snapshot", async () => {
    const index = new WorkIndex({ clock: new ManualClock(0) });
    index.upsert({ id: "ch_1", kind: "agent", status: "running", title: "c", startedAt: 0, countsAsWorker: false, text: "hello" });
    const tool = createTaskOutputTool({ getClient: () => null, getIndex: () => index });
    const read = () => tool.execute("t", { task_id: "ch_1" }, undefined as never, undefined as never, {} as never);
    expect(((await read()).content[0] as { text: string }).text).toContain("hello");
    index.patch("ch_1", { text: "world" });
    expect(((await read()).content[0] as { text: string }).text).toContain("world");
    await expect(read()).rejects.toThrow(/no new output/i);
  });

  it("says in its description that it is not a way to wait", () => {
    const tool = createTaskOutputTool({ getClient: () => null });
    expect(tool.description).toMatch(/not.*wait/i);
  });
});

// Eval batch 1: gpt-6-luna's checks before the wake all went through
// task_list, which answered "running (4s)" and nothing else.
describe("task_list while work runs", () => {
  async function list(status: "running" | "pending" | "completed") {
    const index = new WorkIndex({ clock: new ManualClock(5_000) });
    index.upsert({ id: "sh_1", kind: "shell", status, title: "./build.sh", startedAt: 0, countsAsWorker: true });
    const client = { ensureAvailable: async () => true, isAvailable: () => true, list: async () => [] } as unknown as ManagerClient;
    const tool = createTaskListTool({ getClient: () => client, getIndex: () => index });
    const res = await tool.execute("t", { all: true }, undefined as never, undefined as never, {} as never);
    return (res.content[0] as { text: string }).text;
  }

  it("tells the model to end its turn when something is still running", async () => {
    expect(await list("running")).toMatch(/with no tool call/i);
  });

  it("tells the model to end its turn when something is queued", async () => {
    expect(await list("pending")).toMatch(/with no tool call/i);
  });

  it("says nothing extra when everything has finished", async () => {
    expect(await list("completed")).not.toMatch(/with no tool call/i);
  });
});

// Eval batch 2 (2026-09-29): kimi-for-coding wrote "I'll end my turn now" and
// called another tool in the same message, three times. "End your turn" never
// said what that means in practice: a reply with no tool call.
describe("wait instructions say what ending the turn means", () => {
  it("in the system guidelines", () => {
    expect(BEHAVIOR_GUIDELINES).toMatch(/no tool call/);
    expect(BEHAVIOR_GUIDELINES).toMatch(/does not end/);
  });

  it("in the background notice", () => {
    expect(formatBackgroundNotice("sh_1", "./build.sh", "/tmp/out")).toMatch(/with no tool call/);
  });

  it("in the task_output description", () => {
    expect(createTaskOutputTool({ getClient: () => null }).description).toMatch(/no tool call/);
  });
});
