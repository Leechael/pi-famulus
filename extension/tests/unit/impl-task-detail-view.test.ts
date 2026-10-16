import { describe, expect, it } from "vitest";
import { infoCard, layoutLog, matchRanges, parseQuery, tabRow, type LogView, type TaskTheme } from "../../src/tui/task-detail-view";
import { visibleWidth } from "../../src/tui/pi-tui-load";
import type { WorkItem } from "../../src/work-index";

const plain: TaskTheme = { fg: (_c, t) => t, bold: (t) => t, italic: (t) => t };
const tagged: TaskTheme = {
  fg: (c, t) => `<${c}>${t}</${c}>`,
  bold: (t) => t,
  italic: (t) => t,
  bg: (c, t) => `[${c}]${t}[/]`,
  inverse: (t) => `{${t}}`,
};
const view = (over: Partial<LogView> = {}): LogView => ({ query: "", editing: false, filter: false, showBlank: false, ...over });

const LOG = [
  "GET /api/apps STATUS=200 DUR_MS=2843",
  "",
  "POST /api/verify STATUS=500 DUR_MS=130617",
  "",
  "\x1b[32mGET\x1b[0m /api/apps STATUS=200 DUR_MS=8038\r",
].join("\n");

describe("task output search", () => {
  it("is smart-case with a|b alternatives and merges overlaps", () => {
    const q = parseQuery("verify|status=500")!;
    expect(matchRanges("POST /api/verify STATUS=500", q)).toEqual([[10, 16], [17, 27]]);
    expect(matchRanges("POST /api/verify", parseQuery("Verify")!)).toEqual([]);
    expect(matchRanges("aaaa", parseQuery("aa|aaa")!)).toEqual([[0, 4]]);
    expect(parseQuery(" | ")).toBeNull();
  });

  it("hides blank lines, scrubs control sequences, and keeps a CRLF line", () => {
    const laid = layoutLog(LOG, 80, view(), plain);
    expect(laid.lines).toEqual([
      " GET /api/apps STATUS=200 DUR_MS=2843",
      " POST /api/verify STATUS=500 DUR_MS=130617",
      " GET /api/apps STATUS=200 DUR_MS=8038",
    ]);
    expect([laid.shown, laid.total]).toEqual([3, 5]);
    expect(layoutLog(LOG, 80, view({ showBlank: true }), plain).lines).toHaveLength(5);
  });

  it("highlights every match, marks the current one, or filters to matching lines", () => {
    const hl = layoutLog(LOG, 80, view({ query: "get" }), tagged, 0);
    expect(hl.matches).toEqual([0, 2]);
    expect(hl.lines[0]).toContain("{GET}");
    expect(hl.lines[2]).toContain("[searchMatchBg]<searchMatchText>GET</searchMatchText>[/]");
    const filtered = layoutLog(LOG, 80, view({ query: "status=500", filter: true }), plain);
    expect(filtered.lines).toEqual([" POST /api/verify STATUS=500 DUR_MS=130617"]);
    expect(layoutLog(LOG, 80, view({ query: "zzz", filter: true }), plain).lines).toEqual([" (no line matches)"]);
  });

  it("shows placeholder bodies dimmed and unsearched", () => {
    expect(layoutLog("(no output yet)", 80, view({ query: "out", filter: true }), tagged).lines).toEqual([" <dim>(no output yet)</dim>"]);
  });
});

describe("task detail chrome", () => {
  const item: WorkItem = {
    id: "sh_1",
    kind: "shell",
    status: "failed",
    title: "make build",
    command: "make build",
    cwd: "/tmp/project",
    startedAt: 0,
    endedAt: 5_000,
    exitCode: 2,
    countsAsWorker: true,
    outputPath: "/very/long/path/to/sessions/abc/tasks/sh_1.output",
  };

  it("pins status, exit, command, and where in three rows", () => {
    const card = infoCard(item, 10_000, plain, 120);
    expect(card).toHaveLength(3);
    expect(card[0]).toContain("failed");
    expect(card[0]).toContain("exit 2");
    expect(card[1]).toBe("$ make build");
    expect(card[2]).toContain("cwd /tmp/project");
    for (const line of infoCard(item, 10_000, plain, 40)) expect(visibleWidth(line)).toBeLessThanOrEqual(40);
  });

  it("cuts stream tabs into a rule and reports their click ranges", () => {
    const row = tabRow(["output", "stderr"], "stderr", "3 lines", plain, 60);
    expect(row.line.startsWith("─┐1 output┌┐2 stderr┌")).toBe(true);
    expect(row.line.endsWith("┐3 lines┌─")).toBe(true);
    expect(visibleWidth(row.line)).toBe(60);
    const at = row.line.indexOf("stderr");
    const [x0, x1, tab] = row.hits[1];
    expect(tab).toBe("stderr");
    expect(at >= x0 && at < x1).toBe(true);
  });
});
