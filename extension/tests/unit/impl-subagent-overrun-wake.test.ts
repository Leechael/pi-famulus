import { describe, expect, it } from "vitest";
import { BEHAVIOR_GUIDELINES } from "../../src/behavior-guidelines";
import { formatSubagentOverrun, formatSubagentOverrunBatch, type SubagentOverrunInfo } from "../../src/format";
import { expandedWakeText, registerFamulusMessageRenderers } from "../../src/tui/message-renderers";
import { setPiTuiForTests } from "../../src/tui/pi-tui-load";
import { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN, wakeIds } from "../../src/wake";

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

  it("batches overdue children into one wake with each child's budget", () => {
    const wake = formatSubagentOverrunBatch([
      info(),
      info({
        runId: "run_b",
        childId: "ch_b",
        name: "worker-2",
        elapsedMs: 45 * MIN,
        budgetMs: 40 * MIN,
        nextReminderMs: 5 * MIN,
        hardCeilingMs: 15 * MIN,
        lastActivity: { agoMs: 2 * MIN, text: "Reviewing the migration output." },
        shell: {
          taskId: "sh_2",
          command: "npm run migrate",
          elapsedMs: 7 * MIN,
          outputPath: "/tmp/migration.log",
          outputBytes: 120,
          outputIdleMs: 3_000,
          growing: false,
        },
      }),
    ]);
    expect(wakeIds(wake.details)).toEqual(["ch_a", "ch_b"]);
    expect(wake.details).toMatchObject({
      additional: [{
        runId: "run_b",
        childId: "ch_b",
        elapsedMs: 45 * MIN,
        budgetMs: 40 * MIN,
        nextReminderMs: 5 * MIN,
        hardCeilingMs: 15 * MIN,
        lastActivity: { agoMs: 2 * MIN, text: "Reviewing the migration output." },
        shell: { taskId: "sh_2", command: "npm run migrate" },
      }],
    });
    expect(wake.content).toContain('<child run-id="run_b" child-id="ch_b" name="worker-2" elapsed-ms="2700000" budget-ms="2400000" reminder="1" next-reminder-ms="300000" hard-ceiling-ms="900000">');
    expect(wake.content).toContain('<last-activity ago-ms="120000">Reviewing the migration output.</last-activity>');
    expect(wake.content).toContain('<shell task-id="sh_2" elapsed-ms="420000" output-bytes="120" output-idle-ms="3000" growing="no">');
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
    expect(options).toContain("If you do none of these, it keeps running and the next reminder is scheduled in 10m");
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
    const batched = formatSubagentOverrunBatch([
      info(),
      info({ runId: "run_b", childId: "ch_b", name: "worker-2", lastActivity: { agoMs: 2 * MIN, text: "Inspecting logs." }, hardCeilingMs: 15 * MIN }),
    ]);
    const batchedRender = (expanded: boolean) =>
      (map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(batched, { expanded, outputPad: 0 }, theme).render(160) as string[]).join("\n");
    expect(batchedRender(false)).toContain("+1 more overdue");
    expect(batchedRender(true)).toContain("Also overdue (1)");
    expect(batchedRender(true)).toContain("worker-2 (ch_b) · run run_b");
    expect(batchedRender(true)).toContain("Last activity (2m0s ago): Inspecting logs.");
    expect(batchedRender(true)).toContain("Hard ceiling stops it in 15m0s.");
    expect(expanded).not.toContain("<pi-famulus-wake");
  });
});

describe("subagent-overrun wake under an opt-in hard ceiling (review P2, PR #34)", () => {
  const options = (content: string) => /<options>([\s\S]*)<\/options>/.exec(content)![1];

  it("soft 60s, hard 90s, repeat 10m: says the ceiling stops it in 30s, no promise of another reminder", () => {
    const wake = formatSubagentOverrun(
      info({ elapsedMs: 60_000, budgetMs: 60_000, nextReminderMs: 10 * MIN, hardCeilingMs: 30_000 }),
    );
    expect(wake.content).toContain('reminder="1" hard-ceiling-ms="30000">');
    expect(wake.details).toMatchObject({ hardCeilingMs: 30_000 });
    const text = options(wake.content);
    expect(text).toContain("the configured hard ceiling stops it in 30s, and extend does not move that ceiling");
    expect(text).toContain("If you do none of these, it keeps running until the hard ceiling stops it.");
    expect(text).not.toContain("reminder is scheduled");
    expect(text).not.toContain("It has not been stopped. ");
  });

  it("names the next reminder when it comes before the ceiling", () => {
    const wake = formatSubagentOverrun(info({ nextReminderMs: 10 * MIN, hardCeilingMs: 25 * MIN }));
    expect(options(wake.content)).toContain(
      "If you do none of these, it keeps running and the next reminder is scheduled in 10m, until the hard ceiling stops it in 25m.",
    );
  });

  it("without a ceiling the text and attributes are unchanged", () => {
    const wake = formatSubagentOverrun(info());
    expect(wake.content).not.toContain("hard-ceiling-ms");
    expect(wake.details).not.toHaveProperty("hardCeilingMs");
    expect(options(wake.content)).toContain("If you do none of these, it keeps running and the next reminder is scheduled in 10m");
  });

  it("expanded TUI does not promise a reminder when the hard ceiling is sooner", () => {
    const wake = formatSubagentOverrun(
      info({ elapsedMs: 60_000, budgetMs: 60_000, nextReminderMs: 10 * MIN, hardCeilingMs: 30_000 }),
    );
    const text = expandedWakeText(wake.details, wake.content);
    expect(text).toContain("Hard ceiling stops it in 30.0s (extend does not move it).");
    expect(text).not.toContain("Next reminder");
  });

  it("expanded TUI names the next reminder when it comes before the ceiling", () => {
    const wake = formatSubagentOverrun(info({ nextReminderMs: 10 * MIN, hardCeilingMs: 25 * MIN }));
    const text = expandedWakeText(wake.details, wake.content);
    expect(text).toContain("Hard ceiling stops it in 25m0s (extend does not move it).");
    expect(text).toContain("Next reminder in 10m0s unless you extend, steer, or interrupt it.");
  });
});

describe("behavior guidelines on the overrun wake", () => {
  it("say the subagent was not stopped, name the three actions, and the no-action outcome", () => {
    const bullet = BEHAVIOR_GUIDELINES.split("\n").find((l) => l.includes('kind="subagent-overrun"'));
    expect(bullet).toBeDefined();
    expect(bullet).toContain("is not stopped");
    expect(bullet).toContain("if it is waiting on a foreground shell");
    expect(bullet).toContain('subagent({action:"extend", run_id, child_id, timeout_ms})');
    expect(bullet).toContain("agent_message to steer it");
    expect(bullet).toContain('subagent({action:"interrupt", run_id, child_id})');
    expect(bullet).toContain("If you do none of these it keeps running and the reminder comes again.");
    expect(bullet!.toLowerCase()).not.toContain("keep working");
  });
});
