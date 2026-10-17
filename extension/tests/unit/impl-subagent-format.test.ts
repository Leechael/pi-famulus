import { describe, expect, it } from "vitest";
import { formatSubagentHandover, formatSubagentNotification } from "../../src/format";
import { expandedWakeText } from "../../src/tui/message-renderers";

describe("formatSubagentNotification", () => {
  it("produces one subagent-done child per result", () => {
    const text = formatSubagentNotification({
      runId: "run_a1b2c3d4",
      status: "completed",
      durationMs: 41234,
      children: [
        { childId: "ch_1", name: "worker-1", status: "completed", text: "result one" },
        { childId: "ch_2", name: "worker-2", status: "completed", text: "result two" },
      ],
    }).content;
    expect(text).toContain('kind="subagent-done"');
    expect(text).not.toContain("<subagent-notification>");
    expect(text).toContain("System wake");
    expect(text).toContain('run-id="run_a1b2c3d4"');
    expect(text).toContain('status="completed"');
    expect(text).toContain("<summary>2/2 subagents completed in 41234ms</summary>");
    expect(text).toContain('<child id="ch_1" name="worker-1" status="completed">');
    expect(text).toContain('<child id="ch_2" name="worker-2" status="completed">');
    expect(text).toContain("<result>result one</result>");
  });

  it("summarizes partial runs and includes error lines", () => {
    const text = formatSubagentNotification({
      runId: "run_x",
      status: "partial",
      durationMs: 100,
      children: [
        { childId: "ch_a", name: "a", status: "completed", text: "ok" },
        { childId: "ch_b", name: "b", status: "failed", text: "", error: "boom" },
      ],
    }).content;
    expect(text).toContain("1/2 subagents completed in 100ms");
    expect(text).toContain('<child id="ch_b" name="b" status="failed">');
    expect(text).toContain("<error>boom</error>");
  });

  it("XML-escapes child output", () => {
    const text = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [{ childId: "ch_a", name: "a<b>", status: "completed", text: "x < y & z > w" }],
    }).content;
    expect(text).toContain('name="a&lt;b&gt;"');
    expect(text).toContain("x &lt; y &amp; z &gt; w");
    expect(text).not.toContain("a<b>");
  });

  it("keeps only the 2000-char tail of each child result", () => {
    const long = "x".repeat(3000);
    const text = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [{ childId: "ch_a", name: "a", status: "completed", text: long }],
    }).content;
    expect(text).not.toContain(long);
    expect(text).toContain("x".repeat(2000));
  });

  it("includes each child's prompt in the run results", () => {
    const text = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [{ childId: "ch_a", name: "a", status: "completed", text: "done", prompt: "look at src" }],
    }).content;
    expect(text).toContain("<prompt>look at src</prompt>");
  });
});

describe("formatSubagentHandover", () => {
  it("gives the parent the prompt, the result, and who is still running", () => {
    const text = formatSubagentHandover({
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      prompt: "inspect the loader",
      text: "loader reads mtime",
      stillRunning: [{ id: "ch_2", title: "worker-2" }],
    }).content;
    expect(text).toContain('kind="subagent-handover"');
    expect(text).toContain("<prompt>inspect the loader</prompt>");
    expect(text).toContain("<result>loader reads mtime</result>");
    expect(text).toContain('<item id="ch_2">worker-2</item>');
    expect(text).toContain('child-id="ch_1"');
  });
});

describe("child warnings in subagent wakes", () => {
  const warning = 'unknown thinking level "highest" in model spec "haiku:highest"; using the model\'s default thinking';

  it("subagent-done renders a child's warning before its result", () => {
    const wake = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [
        { childId: "ch_a", name: "a", status: "completed", text: "ok", warning },
        { childId: "ch_b", name: "b", status: "completed", text: "ok" },
      ],
    });
    expect(wake.content).toContain(`<warning>${warning}</warning>\n    <result>ok</result>`);
    expect(wake.content.match(/<warning>/g)).toHaveLength(1);
    expect(wake.details).toMatchObject({ kind: "subagent-done", children: [{ warning }, {}] });
    expect(expandedWakeText(wake.details, wake.content)).toContain(`Warning: ${warning}`);
  });

  it("subagent-handover renders the child's warning before its result", () => {
    const wake = formatSubagentHandover({
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      prompt: "p",
      text: "r",
      warning,
      stillRunning: [{ id: "ch_2", title: "worker-2" }],
    });
    expect(wake.content).toContain(`<warning>${warning}</warning>\n  <result>r</result>`);
    expect(wake.details).toMatchObject({ kind: "subagent-handover", warning });
    expect(expandedWakeText(wake.details, wake.content)).toContain(`Warning: ${warning}`);
  });

  it("omits the element when there is no warning", () => {
    const wake = formatSubagentHandover({
      runId: "run_a", childId: "ch_1", name: "w", status: "completed", prompt: "p", text: "r", stillRunning: [],
    });
    expect(wake.content).not.toContain("<warning>");
    expect(wake.details).not.toHaveProperty("warning");
  });
});

/**
 * A finished child's result is capped to its tail (format.ts capTail). The
 * wake must say so, the same way a monitor wake does (truncated="true" and
 * omitted-chars), and tell the parent how to read the rest. Monitor wakes
 * already do this; subagent-done and subagent-handover currently do not.
 */
describe("subagent result truncation is visible", () => {
  const KEPT = 2000;
  const text = `HEAD${"x".repeat(3000)}TAIL`;
  const omitted = text.length - KEPT;

  it("counts the characters this construction drops", () => {
    expect(text.length).toBe(3008);
    expect(omitted).toBe(1008);
  });

  it.fails("marks a subagent-done child result and says how to read the full text", () => {
    const wake = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [{ childId: "ch_a", name: "a", status: "completed", text, prompt: "inspect" }],
    });
    expectTruncatedResult(wake.content, wake.details as TruncationDetails, "child", omitted);
  });

  it.fails("marks a subagent-handover result and says how to read the full text", () => {
    const wake = formatSubagentHandover({
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      prompt: "inspect",
      text,
      stillRunning: [{ id: "ch_2", title: "worker-2" }],
    });
    expectTruncatedResult(wake.content, wake.details as TruncationDetails, "handover", omitted);
  });

  it.each([2000, 1999])("does not mark a %i-character result", (length) => {
    const exact = "y".repeat(length);
    const done = formatSubagentNotification({
      runId: "run_x",
      status: "completed",
      durationMs: 1,
      children: [{ childId: "ch_a", name: "a", status: "completed", text: exact, prompt: "inspect" }],
    });
    const handover = formatSubagentHandover({
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      prompt: "inspect",
      text: exact,
      stillRunning: [{ id: "ch_2", title: "worker-2" }],
    });
    for (const wake of [done, handover]) {
      expect(wake.content).not.toContain('truncated="true"');
      expect(wake.content).not.toMatch(/omitted-chars=/);
      expect(wake.content).toContain(`<result>${exact}</result>`);
    }
    expect(done.details.kind).toBe("subagent-done");
    if (done.details.kind === "subagent-done") {
      expect(done.details.children[0]).not.toHaveProperty("truncated");
      expect(done.details.children[0]).not.toHaveProperty("omittedChars");
    }
    expect(handover.details.kind).toBe("subagent-handover");
    if (handover.details.kind === "subagent-handover") {
      expect(handover.details).not.toHaveProperty("truncated");
      expect(handover.details).not.toHaveProperty("omittedChars");
    }
  });
});

interface TruncationDetails {
  kind: string;
  result?: string;
  truncated?: boolean;
  omittedChars?: number;
  children?: Array<{ result?: string; truncated?: boolean; omittedChars?: number }>;
}

function expectTruncatedResult(
  content: string,
  details: TruncationDetails,
  where: "child" | "handover",
  omitted: number,
): void {
  const result = content.match(/<result(?:\s[^>]*)?>([\s\S]*?)<\/result>/);
  expect(result?.[1].endsWith("TAIL")).toBe(true);
  expect(result?.[1].includes("HEAD")).toBe(false);
  expect(result?.[1].length).toBe(2000);
  expect(content).toContain('truncated="true"');
  expect(content).toContain(`omitted-chars="${omitted}"`);
  expect(content.includes('action:"get"') || /(?:\/|\.jsonl\b)/.test(content)).toBe(true);
  if (where === "child") {
    expect(details.children?.[0]).toMatchObject({ truncated: true, omittedChars: omitted });
    expect(details.children?.[0]?.result?.endsWith("TAIL")).toBe(true);
  } else {
    expect(details).toMatchObject({ truncated: true, omittedChars: omitted });
    expect(details.result?.endsWith("TAIL")).toBe(true);
  }
}
