import { describe, expect, it } from "vitest";
import {
  backgroundRowText,
  formatBackgroundNotice,
  formatMonitorEvent,
  formatTaskNotification,
  truncateTail,
  type TaskExitInfo,
} from "../../src/format";

describe("truncateTail", () => {
  it("returns text unchanged when within both limits", () => {
    const res = truncateTail("a\nb\nc", 10, 1000);
    expect(res).toEqual({ text: "a\nb\nc", truncated: false, totalLines: 3, totalBytes: 5 });
  });

  it("keeps the tail when the line limit is exceeded", () => {
    const text = ["l1", "l2", "l3", "l4", "l5"].join("\n");
    const res = truncateTail(text, 3, 1000);
    expect(res.text).toBe("l3\nl4\nl5");
    expect(res.truncated).toBe(true);
    expect(res.totalLines).toBe(5);
  });

  it("keeps the tail when the byte limit is exceeded", () => {
    const text = ["aaaa", "bbbb", "cccc", "dddd"].join("\n");
    // 4 lines * 4 bytes + 3 newlines = 19 bytes; limit fits only the last line.
    const res = truncateTail(text, 100, 5);
    expect(res.text).toBe("dddd");
    expect(res.truncated).toBe(true);
    expect(res.totalBytes).toBe(19);
  });

  it("keeps the byte tail of a single line that exceeds maxBytes", () => {
    const res = truncateTail("x".repeat(100), 10, 10);
    expect(res.text).toBe("x".repeat(10));
    expect(res.truncated).toBe(true);
    expect(res.totalLines).toBe(1);
    expect(res.totalBytes).toBe(100);
  });

  it("handles empty input", () => {
    const res = truncateTail("", 10, 10);
    expect(res).toEqual({ text: "", truncated: false, totalLines: 1, totalBytes: 0 });
  });

  it("uses the built-in defaults (2000 lines / 50KB)", () => {
    const manyLines = Array.from({ length: 3000 }, (_, i) => `line${i}`).join("\n");
    const res = truncateTail(manyLines);
    expect(res.truncated).toBe(true);
    expect(res.totalLines).toBe(3000);
    expect(res.text.split("\n")).toHaveLength(2000);
    expect(res.text.endsWith("line2999")).toBe(true);
    expect(res.text.startsWith("line1000\n")).toBe(true);
  });

  it("counts multi-byte characters in bytes, not chars", () => {
    const res = truncateTail("ééé", 10, 4); // each é is 2 bytes
    expect(res.truncated).toBe(true);
    expect(res.totalBytes).toBe(6);
    expect(Buffer.byteLength(res.text, "utf8")).toBeLessThanOrEqual(4);
  });
});

function exitInfo(overrides: Partial<TaskExitInfo> = {}): TaskExitInfo {
  return {
    taskId: "sh_a1b2c3d4",
    kind: "shell",
    command: "npm test",
    status: "completed",
    exitCode: 0,
    durationMs: 12345,
    outputPath: "/home/u/.pi/agent/pi-famulus/sessions/s/tasks/sh_a1b2c3d4.output",
    preview: "all tests passed",
    ...overrides,
  };
}

describe("formatTaskNotification", () => {
  it("matches the §4.5 XML layout for a single event", () => {
    const xml = formatTaskNotification([exitInfo()]).content;
    expect(xml).toContain("System wake");
    expect(xml).toContain('<pi-famulus-wake kind="task">');
    expect(xml).toContain('<task id="sh_a1b2c3d4" kind="shell" status="completed" duration-ms="12345" exit-code="0">');
    expect(xml).toContain('<summary>Background command "npm test" completed (exit code 0)</summary>');
    expect(xml).toContain("<command>npm test</command>");
    expect(xml).toContain("<output-file>/home/u/.pi/agent/pi-famulus/sessions/s/tasks/sh_a1b2c3d4.output</output-file>");
    expect(xml).toContain("<preview>all tests passed</preview>");
  });

  it("merges multiple events into one envelope", () => {
    const xml = formatTaskNotification([
      exitInfo(),
      exitInfo({ taskId: "sh_deadbeef", status: "failed", exitCode: 1, command: "make" }),
    ]).content;
    expect(xml.match(/<pi-famulus-wake /g)).toHaveLength(1);
    expect(xml.match(/<task /g)).toHaveLength(2);
    expect(xml).toContain('id="sh_deadbeef"');
    expect(xml).toContain('status="failed"');
    expect(xml).toContain('Background command "make" failed (exit code 1)');
  });

  it("renders killed and orphaned statuses", () => {
    expect(formatTaskNotification([exitInfo({ status: "killed", exitCode: null })]).content).toContain(
      "was killed",
    );
    expect(formatTaskNotification([exitInfo({ status: "orphaned", exitCode: null })]).content).toContain(
      'status="orphaned"',
    );
  });

  it("escapes XML in command and preview", () => {
    const xml = formatTaskNotification([
      exitInfo({ command: 'grep "<a>&" file', preview: "x < y & z > w" }),
    ]).content;
    expect(xml).toContain('grep "&lt;a&gt;&amp;" file');
    expect(xml).toContain("x &lt; y &amp; z &gt; w");
    expect(xml).not.toContain("x < y");
  });

  it("truncates very long commands in the summary", () => {
    const xml = formatTaskNotification([exitInfo({ command: `cmd ${"x".repeat(200)}` })]).content;
    expect(xml).toContain("…");
    expect(xml.length).toBeLessThan(1500);
  });

  it("names background tasks that are still running", () => {
    const xml = formatTaskNotification([exitInfo()], [{ id: "sh_other", title: "sleep 30" }]).content;
    expect(xml).toContain('<item id="sh_other">sleep 30</item>');
    expect(xml).toContain("<command>");
    expect(xml).toContain("npm test");
  });
});

describe("formatBackgroundNotice", () => {
  it("keeps the model-facing notice actionable", () => {
    const text = formatBackgroundNotice("sh_a1b2c3d4", "npm run build", "/tmp/out.log");
    expect(text).toContain("task_id: sh_a1b2c3d4");
    expect(text).toContain("Output: /tmp/out.log");
    expect(text).toContain("Do not poll");
  });

  it("draws the transcript row from live task state, not from the model text", () => {
    expect(backgroundRowText("sh_1", undefined, 0)).toMatchObject({ glyph: "●", text: "sh_1 running in background · /tasks" });
    expect(backgroundRowText("sh_1", { status: "running", startedAt: 0 }, 5_000).glyph).toBe("●");
    expect(backgroundRowText("sh_1", { status: "completed", exitCode: 0, startedAt: 0, endedAt: 14_000 }, 20_000)).toEqual({
      glyph: "✓",
      color: "success",
      text: "sh_1 finished · exit 0 · 14s",
    });
    expect(backgroundRowText("sh_1", { status: "failed", exitCode: 3, startedAt: 0, endedAt: 12_000 }, 20_000)).toMatchObject({
      glyph: "✗",
      text: "sh_1 failed · exit 3 · 12s · /tasks",
    });
  });
});

describe("formatMonitorEvent", () => {
  it("wraps the batch in a pi-famulus-wake monitor envelope", () => {
    const text = formatMonitorEvent("watch tests", "mon_ab12", "line1\nline2").content;
    expect(text).toContain('<pi-famulus-wake kind="monitor" id="mon_ab12" description="watch tests">');
    expect(text).toContain("System wake");
    expect(text).toContain("<event>line1\nline2</event>");
    expect(text).toContain("</pi-famulus-wake>");
  });

  it("escapes attribute values", () => {
    const text = formatMonitorEvent('a "b" <c>', "mon_x", "body").content;
    expect(text).toContain('description="a &quot;b&quot; &lt;c&gt;"');
  });
});
