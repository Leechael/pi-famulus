import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { formatConversation, turnsFromMessages } from "../../src/subagent/conversation";
import { stderrPathFor } from "../../src/tui/task-output-paths";
import { filterTaskItems, formatWorkRows, groupTaskRows, moveSelection, stopChoice, taskDetailHeader, taskDetailInfo, resolveTaskOutputPath } from "../../src/tui/tasks-command";
import { visibleDetailTabs, wrapLines } from "../../src/tui/scroll-detail-view";
import { WorkIndex, type WorkItem } from "../../src/work-index";

function item(id: string, status: string): WorkItem {
  return {
    id,
    kind: "shell",
    status,
    title: id,
    startedAt: 1,
    countsAsWorker: true,
  };
}

describe("tasks view", () => {
  it("stops the selected id after a reorder, not whatever landed on that index", () => {
    const first = item("sh_1", "running");
    const second = item("sh_2", "running");
    let selected = moveSelection([first, second], first.id, 1);
    expect(selected).toBe("sh_2");
    // Item 1 finishes and drops below the active items. Index 0 is now sh_2.
    const reordered = [second, { ...first, status: "completed", endedAt: 2 }];
    expect(stopChoice(reordered, selected)).toEqual({ action: "stop", id: "sh_2" });
  });

  it("does not stop a finished selection", () => {
    const done = item("sh_2", "completed");
    expect(stopChoice([done], done.id)).toEqual({ action: "already-finished", id: "sh_2" });
  });

  it("lists a running monitor in the overlay rows", () => {
    const index = new WorkIndex({ clock: new ManualClock(10_000) });
    index.upsert({
      id: "mon_abc",
      kind: "monitor",
      status: "running",
      title: "build watcher",
      startedAt: 0,
      countsAsWorker: false,
    });
    const rows = formatWorkRows(index.list(), "mon_abc", 10_000, 80);
    expect(rows.join("\n")).toContain("monitor");
    expect(rows.join("\n")).toContain("build watcher");
    expect(rows.join("\n")).toContain("running");
  });

  it("groups run children and indents the child rows", () => {
    const a = { ...item("ch_a", "running"), kind: "agent" as const, runId: "run_1" };
    const b = { ...item("ch_b", "completed"), kind: "agent" as const, runId: "run_1" };
    expect(groupTaskRows([a, b])).toEqual([
      { type: "run", runId: "run_1", count: 2 },
      { type: "task", item: a, indent: true },
      { type: "task", item: b, indent: true },
    ]);
  });

  it("filters by id, status, command, and end reason", () => {
    const task = { ...item("sh_1", "killed"), title: "compile app", endReason: "timeout" };
    expect(filterTaskItems([task], "compile")).toEqual([task]);
    expect(filterTaskItems([task], "timeout")).toEqual([task]);
    expect(filterTaskItems([task], "missing")).toEqual([]);
  });

  it("shows colored-status glyph, exit reason, and drops kind under 60 columns", () => {
    const task = { ...item("sh_1", "failed"), title: "build", exitCode: 3, endReason: "exited" };
    const narrow = formatWorkRows([task], task.id, 1000, 50)[0];
    expect(narrow).toContain("✗");
    expect(narrow).toContain("exit=3");
    expect(narrow).not.toContain("shell");
  });

  it("shows failed agent error inline in the task row", () => {
    const failed = { ...item("ch_error", "failed"), kind: "agent" as const, error: "529 overloaded_error" };
    expect(formatWorkRows([failed], failed.id, 1000, 100)[0]).toContain("529 overloaded_error");
  });

  it("keeps detail tabs in keyboard shortcut order", () => {
    expect(visibleDetailTabs({ output: () => "", stderr: () => "", info: () => "" }))
      .toEqual(["output", "stderr", "info"]);
    expect(visibleDetailTabs({ conversation: () => "", result: () => "", info: () => "" }))
      .toEqual(["conversation", "result", "info"]);
  });

  it("uses the manager task output path for a live monitor before exit", () => {
    expect(resolveTaskOutputPath(item("mon_1", "running"), "/home/pi-famulus", "sess-a"))
      .toBe(join("/home/pi-famulus", "sessions", "sess-a", "tasks", "mon_1.output"));
  });

  it("puts task identity, outcome, duration, cwd, command, and info in the detail panes", () => {
    const task = {
      ...item("sh_1", "failed"),
      command: "make build",
      cwd: "/tmp/project",
      exitCode: 3,
      outputPath: "/tmp/sh_1.output",
      prompt: "Build the release binary",
      preamble: "Use the project's release checklist",
      error: "exit 3",
    };
    expect(taskDetailHeader(task, 5_001)).toBe("sh_1 · shell · exit 3 · 5s · /tmp/project\n$ make build");
    expect(taskDetailInfo(task, 5_001)).toContain("Output: /tmp/sh_1.output");
    expect(taskDetailInfo(task, 5_001)).toContain("Task prompt (user-authored):\nBuild the release binary");
    expect(taskDetailInfo(task, 5_001)).toContain("Agent preamble (injected):\nUse the project's release checklist");
    expect(taskDetailInfo(task, 5_001)).toContain("Error: exit 3");
  });

  it("shows elapsed time from the task start, not when it was backgrounded", () => {
    const index = new WorkIndex({ clock: new ManualClock(20_000) });
    index.upsert({
      id: "sh_old",
      kind: "shell",
      status: "running",
      title: "sleep 14",
      startedAt: 6_000,
      countsAsWorker: true,
    });
    expect(formatWorkRows(index.list(), "sh_old", 20_000, 80).join("\n")).toContain("14s");
  });

  it("keeps a finished item viewable inside the retain window", () => {
    const index = new WorkIndex({ clock: new ManualClock(1_000), retainMs: 10_000, finishedCap: 50 });
    index.upsert({
      id: "ch_done",
      kind: "agent",
      status: "completed",
      title: "scout",
      startedAt: 0,
      endedAt: 500,
      countsAsWorker: false,
    });
    expect(index.list().map((i) => i.id)).toEqual(["ch_done"]);
  });

  it("hides injected prompt and model preambles from child transcripts", () => {
    const turns = turnsFromMessages([
      { role: "system", content: "CHILD_BEHAVIOR_GUIDELINES hidden" },
      {
        role: "user",
        content: "You are running as model provider/model.\n\nagent preamble\n\n---\n\ninspect the loader",
      },
      { role: "assistant", content: "I will inspect it." },
    ]);
    const text = formatConversation(turns);
    expect(text).toContain("inspect the loader");
    expect(text).not.toContain("CHILD_BEHAVIOR_GUIDELINES");
    expect(text).not.toContain("agent preamble");
    expect(text).not.toContain("You are running as model");
  });

  it("formats a child transcript and the stderr sibling path", () => {
    const text = formatConversation(
      turnsFromMessages([
        { role: "user", content: "look at src" },
        {
          role: "assistant",
          content: [
            { type: "text", text: "reading" },
            { type: "toolCall", name: "read", arguments: { path: "src" } },
          ],
        },
        { role: "toolResult", toolName: "read", content: [{ type: "text", text: "file body" }], isError: false },
      ]),
    );
    expect(text).toContain("── user ──");
    expect(text).toContain("look at src");
    expect(text).toContain("tool read");
    expect(stderrPathFor("/tmp/tasks/sh_ab.output")).toBe("/tmp/tasks/sh_ab.stderr");
  });

  it("wraps ANSI and CJK by visible width", () => {
    const colored = `\x1b[31m${"字".repeat(10)}\x1b[0m`;
    const lines = wrapLines(colored, 4);
    expect(lines.length).toBeGreaterThan(1);
    for (const line of lines) expect(line).not.toMatch(/\uFFFD/);
  });
});

// Manual testing (2026-09-24): a monitor view opened while it ran said
// "running · 57m46s" long after it exited, and a command with no output
// (`true`) showed a bare "(empty)".
describe("task detail tabs follow the live item", () => {
  it("re-reads status from the index and explains empty output", async () => {
    const { mkdtempSync, writeFileSync, rmSync } = await import("node:fs");
    const { tmpdir } = await import("node:os");
    const { join } = await import("node:path");
    const { taskDetailTabs } = await import("../../src/tui/tasks-command");
    const dir = mkdtempSync(join(tmpdir(), "pi-famulus-tabs-"));
    const out = join(dir, "mon_1.output");
    writeFileSync(out, "");
    writeFileSync(join(dir, "mon_1.stderr"), "");
    let item = { id: "mon_1", kind: "monitor" as const, status: "running" as const, title: "noop watcher", command: "true", startedAt: 0, countsAsWorker: false, outputPath: out } as never as import("../../src/work-index").WorkItem;
    const tabs = taskDetailTabs(() => item, { outputPath: out, stderrPath: join(dir, "mon_1.stderr") }, () => 1_000);
    expect(tabs.output()).toContain("running");
    expect(tabs.output()).toContain("(no output yet)");
    item = { ...item, status: "completed", exitCode: 0, endedAt: 5, endReason: "exited" } as typeof item;
    expect(tabs.output()).toContain("exit 0");
    expect(tabs.output()).not.toContain("running");
    expect(tabs.output()).toMatch(/no output; the command printed nothing/);
    expect(tabs.stderr()).toContain("(nothing on stderr)");
    expect(tabs.info()).toContain("noop watcher");
    rmSync(dir, { recursive: true, force: true });
  });
});
