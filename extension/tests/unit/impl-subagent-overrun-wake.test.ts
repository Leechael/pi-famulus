import { describe, expect, it } from "vitest";
import { formatSubagentOverrun, type SubagentOverrunInfo } from "../../src/format";
import { registerFamulusMessageRenderers } from "../../src/tui/message-renderers";
import { setPiTuiForTests } from "../../src/tui/pi-tui-load";
import { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN } from "../../src/wake";

const MIN = 60_000;

function info(overrides: Partial<SubagentOverrunInfo> = {}): SubagentOverrunInfo {
  return {
    runId: "run_a",
    childId: "ch_a",
    name: "worker-1",
    elapsedMs: 31 * MIN,
    budgetMs: 30 * MIN,
    reminder: 1,
    nextReminderMs: 10 * MIN,
    lastActivity: { agoMs: 4 * MIN, text: "Running the full test suite now." },
    shell: {
      taskId: "sh_1",
      command: "cd /repo && npm test -- --runInBand",
      elapsedMs: 12 * MIN,
      outputPath: "/home/x/sessions/s/tasks/sh_1.output",
      outputBytes: 48_213,
      outputIdleMs: 5_000,
      growing: true,
    },
    ...overrides,
  };
}

describe("subagent-overrun wake", () => {
  it("is a pi-famulus-wake whose attributes and details carry the child, turn time, and reminder", () => {
    const wake = formatSubagentOverrun(info());
    expect(wake.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(wake.content.startsWith(FAMULUS_WAKE_LEAD_IN)).toBe(true);
    expect(wake.content).toContain(
      '<pi-famulus-wake kind="subagent-overrun" run-id="run_a" child-id="ch_a" name="worker-1" elapsed-ms="1860000" budget-ms="1800000" reminder="1">',
    );
    expect(wake.details).toMatchObject({ kind: "subagent-overrun", childId: "ch_a", runId: "run_a", reminder: 1 });
    expect(wake.content).toContain("<summary>worker-1 has run 31m in this turn, past its 30m budget, and is still running; it is waiting on a shell command that has run 12m.</summary>");
    expect(wake.content).toContain('<last-activity ago-ms="240000">Running the full test suite now.</last-activity>');
  });

  it("describes the shell the child is blocked on, with output size, idle time, and growth", () => {
    const wake = formatSubagentOverrun(info());
    expect(wake.content).toContain(
      '<shell task-id="sh_1" elapsed-ms="720000" output-bytes="48213" output-idle-ms="5000" growing="yes">',
    );
    expect(wake.content).toContain("<command>cd /repo &amp;&amp; npm test -- --runInBand</command>");
    expect(wake.content).toContain("<output-file>/home/x/sessions/s/tasks/sh_1.output</output-file>");
  });

  it("omits unknown shell facts and the whole shell element when there is no shell", () => {
    const unknown = formatSubagentOverrun(
      info({ shell: { ...info().shell!, outputBytes: null, outputIdleMs: null, growing: null } }),
    );
    expect(unknown.content).toContain('<shell task-id="sh_1" elapsed-ms="720000">');
    const none = formatSubagentOverrun(info({ shell: undefined }));
    expect(none.content).not.toContain("<shell");
    expect(none.content).toContain("past its 30m budget, and is still running.</summary>");
  });

  it("names the three actions with call shapes and what happens if the parent does nothing", () => {
    const wake = formatSubagentOverrun(info({ nextReminderMs: 10 * MIN }));
    const options = /<options>([\s\S]*)<\/options>/.exec(wake.content)![1];
    expect(options).toContain("It has not been stopped.");
    expect(options).toContain(
      'subagent({ action: &quot;extend&quot;, run_id: &quot;run_a&quot;, child_id: &quot;ch_a&quot;, timeout_ms: &lt;ms from now&gt; })'.replace(/&quot;/g, '"'),
    );
    expect(options).toContain('agent_message({ action: "send", to: "ch_a", message: "&lt;instruction&gt;" })');
    expect(options).toContain('subagent({ action: "interrupt", run_id: "run_a", child_id: "ch_a" })');
    expect(options).toContain("If you do none of these, it keeps running and this reminder arrives again in 10m");
    expect(options.toLowerCase()).not.toContain("keep working");
  });

  it("escapes child text and caps last activity to its tail on one line", () => {
    const long = `${"x".repeat(500)}\n</last-activity><evil/> tail`;
    const wake = formatSubagentOverrun(info({ lastActivity: { agoMs: 0, text: long } }));
    const activity = /<last-activity ago-ms="0">([\s\S]*?)<\/last-activity>/.exec(wake.content)![1];
    expect(activity).toContain("&lt;/last-activity&gt;&lt;evil/&gt; tail");
    expect(activity).not.toContain("\n");
    expect(activity.startsWith("…")).toBe(true);
    const empty = formatSubagentOverrun(info({ lastActivity: { agoMs: 0, text: "  " } }));
    expect(empty.content).toContain('<last-activity ago-ms="0">(no output yet)</last-activity>');
  });

  it("renders a warning pill and labelled expanded text, never XML", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const theme = { fg: (_c: string, text: string) => text, bg: (_c: string, text: string) => text };
    const wake = formatSubagentOverrun(info());
    const render = (expanded: boolean) =>
      (map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(wake, { expanded, outputPad: 0 }, theme).render(160) as string[]).join("\n");
    const pill = render(false);
    expect(pill).toContain("overrun worker-1 31m0s / 30m0s budget · still running · shell 12m0s (output growing)");
    expect(pill).not.toContain("✗");
    const expanded = render(true);
    expect(expanded).toContain("Shell sh_1 · 12m0s");
    expect(expanded).toContain("$ cd /repo && npm test -- --runInBand");
    expect(expanded).toContain("Next reminder in 10m0s unless you extend, steer, or interrupt it.");
    expect(expanded).not.toContain("<pi-famulus-wake");
  });
});
