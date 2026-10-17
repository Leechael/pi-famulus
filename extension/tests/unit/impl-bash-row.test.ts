import { describe, expect, it } from "vitest";
import { createBashOverride } from "../../src/bash-override";
import { ManualClock } from "../../src/clock";
import { WorkIndex } from "../../src/work-index";

const theme = { fg: (_c: string, t: string) => t, bg: (_c: string, t: string) => t, bold: (t: string) => t } as never;

function render(tool: ReturnType<typeof createBashOverride>, result: unknown, context: { state: object; invalidate: () => void }, width = 80, expanded = false) {
  const component = tool.renderResult!(result as never, { expanded, isPartial: false }, theme, context as never) as unknown as {
    render(w: number): string[];
  };
  return component.render(width);
}

describe("bash transcript row", () => {
  it("shows one line for a backgrounded command and redraws when it finishes", () => {
    const clock = new ManualClock(0);
    const index = new WorkIndex({ clock });
    index.upsert({ id: "sh_1", kind: "shell", status: "running", title: "npm test", startedAt: 0, countsAsWorker: true });
    const tool = createBashOverride({
      getClient: () => null,
      config: { foregroundBudgetMs: 20_000 } as never,
      home: "/tmp",
      sessionId: () => "s",
      sessionEnv: () => ({}),
      trackTask: () => {},
      markNotifyOnExit: () => {},
      clock,
      getIndex: () => index,
    });
    let invalidations = 0;
    const context = { state: {}, invalidate: () => invalidations++ };
    const result = {
      content: [{ type: "text", text: 'Command "npm test" moved to background (task_id: sh_1). Output: /x. Do not poll…' }],
      details: { backgrounded: true, task_id: "sh_1", fullOutputPath: "/x" },
    };
    const first = render(tool, result, context);
    expect(first).toEqual(["● sh_1 running in background · /tasks"]);

    index.patch("sh_1", { status: "failed", exitCode: 1, endedAt: 3_000 });
    expect(invalidations).toBeGreaterThan(0);
    expect(render(tool, result, context)).toEqual(["✗ sh_1 failed · exit 1 · 3s · /tasks"]);

    const after = invalidations;
    index.upsert({ id: "sh_2", kind: "shell", status: "running", title: "x", startedAt: 0, countsAsWorker: true });
    expect(invalidations).toBe(after); // stopped listening once terminal
  });

  it("collapses ordinary output to 10 lines like pi's default view", () => {
    const tool = createBashOverride({
      getClient: () => null,
      config: { foregroundBudgetMs: 20_000 } as never,
      home: "/tmp",
      sessionId: () => "s",
      sessionEnv: () => ({}),
      trackTask: () => {},
      markNotifyOnExit: () => {},
    });
    const text = Array.from({ length: 25 }, (_, i) => `line ${i + 1}`).join("\n");
    const result = { content: [{ type: "text", text }], details: undefined };
    const collapsed = render(tool, result, { state: {}, invalidate: () => {} });
    expect(collapsed).toHaveLength(11);
    expect(collapsed.at(-1)).toContain("15 more lines");
    expect(render(tool, result, { state: {}, invalidate: () => {} }, 80, true)).toHaveLength(25);
  });
});
