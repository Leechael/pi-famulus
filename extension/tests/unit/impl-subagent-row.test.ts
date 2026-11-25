import { describe, expect, it } from "vitest";
import type { TuiMouseEvent } from "@earendil-works/pi-tui";
import { ManualClock } from "../../src/clock";
import type { RunRecord } from "../../src/subagent/registry";
import { createSubagentTool } from "../../src/subagent/tool";
import { WorkIndex } from "../../src/work-index";

const theme = { fg: (_c: string, t: string) => t, bg: (_c: string, t: string) => t, bold: (t: string) => t } as never;

const leftClick: TuiMouseEvent = {
  type: "click", button: "left", x: 0, y: 0, screenX: 0, screenY: 0,
  width: 80, height: 1, shift: false, alt: false, ctrl: false,
};

const callTool = createSubagentTool({
  getRegistry: () => null,
  getNotifyCenter: () => null,
  budgetMs: () => 45_000,
  defaultTimeoutMs: 1_000,
  defaultConcurrency: 4,
});

type CallRenderer = NonNullable<typeof callTool.renderCall>;

function callRow(args: Parameters<CallRenderer>[0]) {
  let component: ReturnType<CallRenderer>;
  // Supply fields required by newer Pi without excess-property errors on 1.0.0.
  const rendererOptions = { durationMs: undefined, outputPad: 1, keyText: () => "", keyHint: () => "" };
  const context: Parameters<CallRenderer>[2] = {
    ...rendererOptions,
    args, toolCallId: "call", lastComponent: undefined, state: {}, cwd: "/tmp",
    executionStarted: true, argsComplete: true, isPartial: true,
    expanded: false, showImages: false, isError: false,
    invalidate: () => {
      context.lastComponent = component;
      component = callTool.renderCall!(context.args, theme, context);
    },
  };
  context.invalidate();
  return {
    render: (width = 80) => component.render(width),
    mouse: (event: TuiMouseEvent) => component.handleMouse?.(event),
    refresh: (update: Partial<typeof context>) => {
      Object.assign(context, update);
      context.invalidate();
    },
  };
}

describe("subagent transcript row", () => {
  it("opens and closes one call's arguments even before it has a result", () => {
    const args = {
      model: "demo/model", concurrency: 2,
      tasks: [{ prompt: "Compare the displayed totals" }, { prompt: "Check whether the labels match" }],
    };
    const row = callRow(args);
    const sibling = callRow(args);
    expect(row.render()).toEqual(["▸ subagent · 2 tasks · concurrency 2"]);
    expect(row.mouse(leftClick)?.handled).toBe(true);
    expect(row.render()[0]).toBe("▾ subagent · 2 tasks · concurrency 2");
    expect(row.render().join("\n")).toContain('"model": "demo/model"');
    expect(row.render().join("\n")).toContain("Check whether the labels match");
    expect(sibling.render()).toEqual(["▸ subagent · 2 tasks · concurrency 2"]);
    row.refresh({ isPartial: false });
    expect(row.render().join("\n")).toContain("Check whether the labels match");
    expect(row.mouse(leftClick)?.handled).toBe(true);
    expect(row.render()).toEqual(["▸ subagent · 2 tasks · concurrency 2"]);
  });

  it("follows Pi's keyboard expansion changes after a local click", () => {
    const row = callRow({ action: "steer", child_id: "ch_alpha", message: "Please check the labels" });
    expect(row.render()).toEqual(["▸ subagent · steer"]);
    row.refresh({ expanded: true });
    expect(row.render().join("\n")).toContain("Please check the labels");
    row.mouse(leftClick);
    expect(row.render()).toEqual(["▸ subagent · steer"]);
    row.refresh({ expanded: false });
    row.refresh({ expanded: true });
    expect(row.render().join("\n")).toContain("Please check the labels");
    row.refresh({ expanded: false });
    expect(row.render()).toEqual(["▸ subagent · steer"]);
  });

  it("shows one line per child for a backgrounded run and redraws as children finish", () => {
    const clock = new ManualClock(10_000);
    const index = new WorkIndex({ clock });
    const record: RunRecord = {
      runId: "run_1",
      kind: "tasks",
      status: "running",
      createdAt: 0,
      children: [
        { childId: "ch_a", name: "alpha", agent: "worker", status: "running", startedAt: 5_000 },
        { childId: "ch_b", name: "beta", agent: "worker", status: "failed", startedAt: 5_000, endedAt: 7_000, result: { status: "failed", text: "", error: "529 overloaded", durationMs: 2_000 } },
      ],
    };
    const tool = createSubagentTool({
      getRegistry: () => ({ get: () => record }) as never,
      getNotifyCenter: () => null,
      getIndex: () => index,
      budgetMs: () => 45_000,
      defaultTimeoutMs: 1_000,
      defaultConcurrency: 4,
      clock,
    });
    let invalidations = 0;
    const context = { state: {}, invalidate: () => invalidations++ };
    const result = {
      content: [{ type: "text", text: "Started 2 subagent(s) in run run_1. … Do not poll." }],
      details: { run_id: "run_1", status: "backgrounded" },
    };
    const render = () =>
      (tool.renderResult!(result as never, { expanded: false, isPartial: false }, theme, context as never) as unknown as {
        render(w: number): string[];
      }).render(80);

    const lines = render();
    expect(lines[0]).toBe("run run_1 · running · /tasks");
    expect(lines[1]).toBe("  ● alpha running 5.0s");
    expect(lines[2]).toBe("  ✗ beta failed 2.0s · 529 overloaded");
    expect(lines.join("\n")).not.toContain("Do not poll");

    index.upsert({ id: "ch_a", kind: "agent", status: "completed", title: "alpha", startedAt: 5_000, endedAt: 9_000, countsAsWorker: false });
    expect(invalidations).toBeGreaterThan(0);
  });
});
