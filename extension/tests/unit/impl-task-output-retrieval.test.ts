import { describe, expect, it } from "vitest";
import type { ManagerClient } from "../../src/manager-client";
import { createTaskOutputTool } from "../../src/task-tools";
import { WorkIndex } from "../../src/work-index";

// External manager boundary: offsets/counts are original UTF-8 bytes, not JS
// character positions. ASCII page ends below avoid splitting a UTF-8 character.
function setup(text: string) {
  let bytes = Buffer.from(text);
  let status = "running";
  const requests: Array<{ id: string; cursor: number; maxBytes: number }> = [];
  const client = {
    ensureAvailable: async () => true,
    output: async (id: string, cursor: number, maxBytes: number) => {
      requests.push({ id, cursor, maxBytes });
      const end = Math.max(cursor, Math.min(bytes.length, cursor + Math.min(maxBytes, 1024 * 1024)));
      return {
        chunk: bytes.subarray(cursor, end).toString("utf8"),
        next_cursor: end,
        status,
        exit_code: status === "completed" ? 0 : null,
        total_size: bytes.length,
      };
    },
  } as unknown as ManagerClient;
  const tool = createTaskOutputTool({ getClient: () => client });
  const read = (params: { task_id?: string; cursor?: number; max_bytes?: number } = {}) =>
    tool.execute("t", { task_id: "sh_monitor", ...params }, undefined as never, undefined as never, {} as never);
  return {
    tool, read, requests,
    append: (text: string) => { bytes = Buffer.concat([bytes, Buffer.from(text)]); },
    complete: () => { status = "completed"; },
  };
}

const outputText = (result: Awaited<ReturnType<ReturnType<typeof setup>["read"]>>) =>
  (result.content[0] as { text: string }).text;

describe("task_output original-byte retrieval", () => {
  it.each(["cursor", "max_bytes"] as const)("invariant: rejects invalid %s before querying the manager", async (field) => {
    for (const value of [-1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, Number.MAX_SAFE_INTEGER + 1]) {
      const { read, requests } = setup("original\n");
      await expect(read({ [field]: value })).rejects.toThrow(
        new TypeError(`${field} must be a nonnegative safe integer`),
      );
      expect(requests).toEqual([]);
      // A rejected input must not consume the first legitimate read.
      const valid = await read({ cursor: 0, max_bytes: 9 });
      expect(outputText(valid).split("\n\n[task")[0]).toBe("original\n");
      expect(valid.details).toMatchObject({ cursor: 0, next_cursor: 9, total_size: 9 });
    }
  });

  it("regression: clamps the limit before calculating the tail so the actual manager cap cannot hide the end", async () => {
    const { read, requests } = setup("prefix" + "x".repeat(1024 * 1024) + "END\n");
    const result = await read({ max_bytes: 2 * 1024 * 1024 });
    const body = outputText(result).split("\n\n[task")[0];
    expect(body.length).toBe(1024 * 1024);
    expect(body.endsWith("END\n")).toBe(true);
    expect(result.details).toEqual({
      task_id: "sh_monitor", status: "running", exit_code: null,
      cursor: 10, next_cursor: 1048586, total_size: 1048586,
    });
    expect(requests).toEqual([
      { id: "sh_monitor", cursor: 0, maxBytes: 1 },
      { id: "sh_monitor", cursor: 10, maxBytes: 1048576 },
    ]);
    const head = await read({ cursor: 0, max_bytes: Number.MAX_SAFE_INTEGER });
    expect(outputText(head).startsWith("prefix")).toBe(true);
    expect(head.details).toMatchObject({ cursor: 0, next_cursor: 1048576 });
    expect(requests.at(-1)).toEqual({ id: "sh_monitor", cursor: 0, maxBytes: 1048576 });
  });

  it.each([undefined, 0])("invariant: preserves the historical default tail window for max_bytes=%s", async (max_bytes) => {
    const { read } = setup("head" + "z".repeat(65536));
    const result = await read({ max_bytes });
    expect(outputText(result).split("\n\n[task")[0]).toBe("z".repeat(65536));
    expect(result.details).toMatchObject({ cursor: 4, next_cursor: 65540, total_size: 65540 });
  });

  it("regression: pages a running monitor's unchanged original bytes with precise cursors", async () => {
    const { read } = setup("α\nfirst\nsecond\n");
    const first = await read({ cursor: 0, max_bytes: 9 });
    expect(outputText(first).split("\n\n[task")[0]).toBe("α\nfirst\n");
    expect(first.details).toEqual({
      task_id: "sh_monitor", status: "running", exit_code: null,
      cursor: 0, next_cursor: 9, total_size: 16,
    });
    const second = await read({ cursor: first.details.next_cursor, max_bytes: 7 });
    expect(outputText(second).split("\n\n[task")[0]).toBe("second\n");
    expect(second.details).toEqual({
      task_id: "sh_monitor", status: "running", exit_code: null,
      cursor: 9, next_cursor: 16, total_size: 16,
    });
  });

  it("invariant: remembers earlier pages instead of allowing alternating unchanged reads", async () => {
    const { read } = setup("first\nsecond\n");
    await read({ cursor: 0, max_bytes: 6 });
    await read({ cursor: 6, max_bytes: 7 });
    await expect(read({ cursor: 0, max_bytes: 6 })).rejects.toThrow(/no new output/i);
    await expect(read({ cursor: 6, max_bytes: 7 })).rejects.toThrow(/no new output/i);
  });

  it.each([false, true])("invariant: tail and explicit reads of the same returned range cannot bypass the guard (tail first: %s)", async (tailFirst) => {
    const { read } = setup("first\nsecond\n");
    const tail = { max_bytes: 7 };
    const explicit = { cursor: 6, max_bytes: 100 };
    const first = await read(tailFirst ? tail : explicit);
    expect(outputText(first)).toContain("second\n");
    expect(first.details).toMatchObject({ cursor: 6, next_cursor: 13 });
    await expect(read(tailFirst ? explicit : tail)).rejects.toThrow(/no new output/i);
  });

  it("invariant: refreshes page history on growth and allows repeated reads after completion", async () => {
    const { read, append, complete } = setup("first\nsecond\n");
    await read({ cursor: 0, max_bytes: 6 });
    await read({ cursor: 6, max_bytes: 7 });
    append("third\n");
    const firstAgain = await read({ cursor: 0, max_bytes: 6 });
    expect(outputText(firstAgain).split("\n\n[task")[0]).toBe("first\n");
    expect(firstAgain.details).toMatchObject({ cursor: 0, next_cursor: 6, total_size: 19 });
    const last = await read({ cursor: 13, max_bytes: 6 });
    expect(outputText(last).split("\n\n[task")[0]).toBe("third\n");
    expect(last.details).toMatchObject({ cursor: 13, next_cursor: 19 });
    await expect(read({ cursor: 13, max_bytes: 6 })).rejects.toThrow(/no new output/i);
    complete();
    for (let i = 0; i < 2; i++) {
      const result = await read({ cursor: 13, max_bytes: 6 });
      expect(outputText(result).split("\n\n[task")[0]).toBe("third\n");
      expect(result.details).toMatchObject({ status: "completed", exit_code: 0, cursor: 13, next_cursor: 19 });
    }
  });

  it("characterization: child output stays full-text and cannot bypass polling by changing pagination", async () => {
    const index = new WorkIndex();
    index.upsert({ id: "ch_1", kind: "agent", status: "pending", title: "child", startedAt: Date.now(), countsAsWorker: false, text: "child output" });
    const tool = createTaskOutputTool({ getClient: () => null, getIndex: () => index });
    const read = (cursor: number, max_bytes: number) => tool.execute("t", { task_id: "ch_1", cursor, max_bytes }, undefined as never, undefined as never, {} as never);
    const first = await read(100, 1);
    expect(outputText(first)).toMatch(/^child output\n\n/);
    expect(first.details).toMatchObject({ task_id: "ch_1", status: "pending", cursor: 0, next_cursor: 0 });
    await expect(read(0, 2)).rejects.toThrow(/no new output/i);
    index.patch("ch_1", { status: "running", text: "child output\nmore", error: "failed step" });
    const grown = await read(100, 1);
    expect(outputText(grown)).toMatch(/^child output\nmore\n\nError: failed step\n\n/);
    index.patch("ch_1", { status: "completed" });
    for (let i = 0; i < 2; i++) {
      const result = await read(0, 1);
      expect(outputText(result)).toBe("child output\nmore\n\nError: failed step");
      expect(result.details).toMatchObject({ status: "completed", cursor: 0, next_cursor: 0 });
    }
  });

  it("regression: changing an empty read's cursor cannot turn no-progress polling into retrieval", async () => {
    const { read } = setup("done\n");
    const first = await read({ cursor: 5, max_bytes: 1 });
    expect(outputText(first)).toContain("(no output yet)");
    expect(first.details).toMatchObject({ cursor: 5, next_cursor: 5, total_size: 5 });
    await expect(read({ cursor: 500, max_bytes: 2 })).rejects.toThrow(/no new output/i);
  });
});
