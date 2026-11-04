import { describe, expect, it } from "vitest";
import { formatSupervisorRequest } from "../../src/comms/comms";
import {
  formatMonitorEvent,
  formatMonitorTerminal,
  formatSubagentHandover,
  formatSubagentNotification,
  formatTaskNotification,
  MONITOR_UNWATCHED_SUMMARY,
  type TaskExitInfo,
} from "../../src/format";
import { formatFamulusWake, stampWakeAsOf, wakeIds, FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN } from "../../src/wake";
import { onWakeMessageEnd } from "../../src/wake-delivery";
import { registerFamulusMessageRenderers } from "../../src/tui/message-renderers";
import { setPiTuiForTests, visibleWidth } from "../../src/tui/pi-tui-load";

describe("pi-famulus-wake envelope", () => {
  it("uses the literal Famulus custom type and XML at every public formatter seam", () => {
    const wakes = [
      formatFamulusWake({ kind: "supervisor-update", from: "ch_a", name: "worker", message: "ready" }),
      formatTaskNotification([exit()]),
      formatMonitorEvent("watch", "mon_1", "tick"),
      formatSubagentHandover({ runId: "run_a", childId: "ch_a", name: "worker", status: "completed", stillRunning: [], prompt: "look", text: "done" }),
      formatSubagentNotification({ runId: "run_a", status: "completed", durationMs: 1, children: [] }),
      formatSupervisorRequest({ childId: "ch_a", name: "worker" }, "Which file?"),
    ];
    for (const wake of wakes) {
      expect(wake.customType).toBe("pi-famulus-wake");
      expect(wake.content).toContain(`<pi-famulus-wake kind="${wake.details.kind}"`);
      expect(wake.content).toContain("</pi-famulus-wake>");
    }
  });

  it("renders details.asOf as the last root attribute, UTC to the second", () => {
    const wake = formatFamulusWake({
      kind: "supervisor-update",
      from: "ch_a",
      name: "worker",
      message: "ready",
      asOf: Date.UTC(2026, 9, 5, 12, 27, 26, 999),
    });
    expect(wake.content).toContain(
      '<pi-famulus-wake kind="supervisor-update" from="ch_a" name="worker" as-of="2026-10-05T12:27:26Z">',
    );
  });

  it("stampWakeAsOf leaves details unstamped when content has no root tag", () => {
    const asOf = Date.UTC(2026, 9, 5, 12, 27, 26, 61);
    const wake = formatFamulusWake({
      kind: "supervisor-update",
      from: "ch_a",
      name: "worker",
      message: "ready",
    });
    const broken = stampWakeAsOf(
      { customType: wake.customType, content: "no wake tag here", details: { ...wake.details } },
      asOf,
    );
    expect(broken.content).toBe("no wake tag here");
    expect((broken.details as { asOf?: number }).asOf).toBeUndefined();
    const stamped = stampWakeAsOf(
      { customType: wake.customType, content: wake.content, details: { ...wake.details } },
      asOf,
    );
    expect(stamped.content).toContain('as-of="2026-10-05T12:27:26Z"');
    expect((stamped.details as { asOf?: number }).asOf).toBe(asOf);
  });

  it("wraps a task batch in one envelope and keeps a comma inside an item title", () => {
    const wake = formatFamulusWake({
      kind: "task",
      stillRunning: [{ id: "sh_other", title: "npm test, coverage" }],
      tasks: [
        {
          id: "sh_a1b2c3d4",
          taskKind: "shell",
          status: "completed",
          summary: 'Background command "npm test" completed (exit code 0)',
          command: "npm test",
          outputPath: "/tmp/sh_a1b2c3d4.output",
          preview: "all tests passed",
          durationMs: 12345,
          exitCode: 0,
        },
        {
          id: "sh_dead",
          taskKind: "shell",
          status: "failed",
          summary: 'Background command "make" failed (exit code 1)',
          command: "make",
          outputPath: "/tmp/sh_dead.output",
          preview: "",
          durationMs: 12345,
          exitCode: 1,
        },
      ],
    });
    expect(wake.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(wake.content.startsWith(FAMULUS_WAKE_LEAD_IN)).toBe(true);
    expect(wake.content.match(/<pi-famulus-wake /g)).toHaveLength(1);
    expect(wake.content).toContain('<pi-famulus-wake kind="task">');
    expect(wake.content).toContain('<item id="sh_other">npm test, coverage</item>');
    expect(wake.content).not.toContain("<still-running>npm test, coverage");
    expect(wake.content).toContain(
      '<task id="sh_a1b2c3d4" kind="shell" status="completed" duration-ms="12345" exit-code="0">',
    );
    expect(wake.content).toContain('<task id="sh_dead" kind="shell" status="failed" duration-ms="12345" exit-code="1">');
    expect(wake.details.kind).toBe("task");
  });

  it("omits still-running and exit-code when empty or null, and keeps signal as a name", () => {
    const wake = formatFamulusWake({
      kind: "task",
      stillRunning: [],
      tasks: [
        {
          id: "sh_killed",
          taskKind: "shell",
          status: "killed",
          summary: 'Background command "npm test" was killed',
          command: "npm test",
          outputPath: "/tmp/x",
          preview: "",
          durationMs: 10,
          exitCode: null,
          signal: "SIGTERM",
        },
      ],
    });
    expect(wake.content).not.toContain("<still-running>");
    expect(wake.content).not.toContain("exit-code");
    expect(wake.content).toContain('signal="SIGTERM"');
    if (wake.details.kind !== "task") throw new Error("kind");
    expect(wake.details.tasks[0].exitCode).toBeNull();
    expect(wake.details.tasks[0].signal).toBe("SIGTERM");
  });

  it("still parses when the lead-in is ablated to empty", () => {
    const wake = formatFamulusWake(
      {
        kind: "task",
        stillRunning: [],
        tasks: [
          {
            id: "sh_1",
            taskKind: "shell",
            status: "completed",
            summary: "done",
            command: "true",
            outputPath: "/tmp/x",
            preview: "",
            durationMs: 1,
            exitCode: 0,
          },
        ],
      },
      "",
    );
    expect(wake.content.startsWith("<pi-famulus-wake ")).toBe(true);
    expect(wake.content).not.toContain(FAMULUS_WAKE_LEAD_IN);
  });

  it("escapes a monitor event body, including a fake closing tag", () => {
    const wake = formatFamulusWake({
      kind: "monitor",
      id: "mon_1",
      description: "watch <tests>",
      status: "exited",
      event: "line <a>\n</event>\nline2",
    });
    expect(wake.content).toContain(
      '<pi-famulus-wake kind="monitor" id="mon_1" description="watch &lt;tests&gt;" status="exited">',
    );
    expect(wake.content).toContain("<event>line &lt;a&gt;\n&lt;/event&gt;\nline2</event>");
    expect(wake.content).not.toContain("<event>line <a>");
    expect(wake.details).toMatchObject({ kind: "monitor", event: "line <a>\n</event>\nline2" });
  });

  it("emits subagent-done as one child element per child", () => {
    const wake = formatFamulusWake({
      kind: "subagent-done",
      runId: "run_a",
      status: "partial",
      durationMs: 100,
      summary: "1/2 subagents completed in 100ms",
      children: [
        { childId: "ch_1", name: "a", status: "completed", prompt: "look", result: "ok" },
        { childId: "ch_2", name: "b", status: "failed", prompt: "fix", result: "", error: "boom" },
      ],
    });
    expect(wake.content).toContain(
      '<pi-famulus-wake kind="subagent-done" run-id="run_a" status="partial" duration-ms="100">',
    );
    expect(wake.content).toContain('<child id="ch_2" name="b" status="failed">');
    expect(wake.content).toContain("<error>boom</error>");
    expect(wake.content).toContain("<prompt>look</prompt>");
    expect(wake.details).toMatchObject({
      kind: "subagent-done",
      children: [
        { childId: "ch_1", prompt: "look", result: "ok" },
        { childId: "ch_2", error: "boom" },
      ],
    });
  });

  it("puts handover still-running titles in item elements", () => {
    const wake = formatFamulusWake({
      kind: "subagent-handover",
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      stillRunning: [{ id: "ch_2", title: "reviewer, slow" }],
      summary: "worker-1 completed; 1 still running",
      prompt: "inspect",
      result: "done",
    });
    expect(wake.content).toContain('kind="subagent-handover"');
    expect(wake.content).toContain('<item id="ch_2">reviewer, slow</item>');
    expect(wake.content).toContain("<prompt>inspect</prompt>");
    expect(wake.content).toContain("<result>done</result>");
  });

  it("puts the reply recipe in reply-with, not after the message", () => {
    const wake = formatFamulusWake({
      kind: "supervisor-request",
      from: "ch_a",
      name: "explorer",
      message: "Which file?",
    });
    expect(wake.content).toContain('<pi-famulus-wake kind="supervisor-request" from="ch_a" name="explorer">');
    expect(wake.content).toContain("<message>Which file?</message>");
    expect(wake.content).toContain(
      '<reply-with>agent_message { action: "reply", to: "ch_a", message: "&lt;your decision&gt;" }</reply-with>',
    );
    const message = wake.content.slice(wake.content.indexOf("<message>"), wake.content.indexOf("</message>"));
    expect(message).not.toContain("action:");
  });
});

const exit = (overrides: Partial<TaskExitInfo> = {}): TaskExitInfo => ({
  taskId: "sh_a1b2c3d4",
  kind: "shell",
  command: "npm test",
  status: "completed",
  exitCode: 0,
  durationMs: 12345,
  outputPath: "/tmp/sh_a1b2c3d4.output",
  preview: "all tests passed",
  ...overrides,
});

describe("pi-famulus-wake envelope", () => {
  it("wraps a task batch in one envelope and keeps a comma inside an item title", () => {
    const wake = formatTaskNotification(
      [exit(), exit({ taskId: "sh_dead", status: "failed", exitCode: 1, command: "make" })],
      [{ id: "sh_other", title: "npm test, coverage" }],
    );
    expect(wake.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(wake.content.startsWith(FAMULUS_WAKE_LEAD_IN)).toBe(true);
    expect(wake.content.match(/<pi-famulus-wake /g)).toHaveLength(1);
    expect(wake.content).toContain('<pi-famulus-wake kind="task">');
    expect(wake.content).toContain('<item id="sh_other">npm test, coverage</item>');
    expect(wake.content).not.toContain("<still-running>npm test, coverage");
    expect(wake.content).toContain('<task id="sh_a1b2c3d4" kind="shell" status="completed" duration-ms="12345" exit-code="0">');
    expect(wake.content).toContain('<task id="sh_dead" kind="shell" status="failed" duration-ms="12345" exit-code="1">');
    expect(wake.details).toMatchObject({
      kind: "task",
      stillRunning: [{ id: "sh_other", title: "npm test, coverage" }],
      tasks: [
        { id: "sh_a1b2c3d4", exitCode: 0 },
        { id: "sh_dead", exitCode: 1, status: "failed" },
      ],
    });
  });

  it("omits still-running and exit-code when empty or null, and keeps signal as a name", () => {
    const wake = formatTaskNotification([
      exit({ status: "killed", exitCode: null, signal: "SIGTERM" }),
    ]);
    expect(wake.content).not.toContain("<still-running>");
    expect(wake.content).not.toContain("exit-code");
    expect(wake.content).toContain('signal="SIGTERM"');
    expect(wake.details.kind).toBe("task");
    if (wake.details.kind !== "task") return;
    expect(wake.details.tasks[0].exitCode).toBeNull();
    expect(wake.details.tasks[0].signal).toBe("SIGTERM");
  });

  it("still parses when the lead-in is ablated to empty", () => {
    const wake = formatTaskNotification([exit()], [], "");
    expect(wake.content.startsWith("<pi-famulus-wake ")).toBe(true);
    expect(wake.content).not.toContain(FAMULUS_WAKE_LEAD_IN);
    expect(wake.details.kind).toBe("task");
  });

  it("renders dropped-line and event-count metadata on monitor wakes", () => {
    const wake = formatMonitorEvent("watch tests", "mon_1", "2 events · last: tick", undefined, {
      eventCount: 2,
      droppedLines: 7,
    });
    expect(wake.content).toContain('event-count="2" dropped-lines="7"');
    expect(wake.details).toMatchObject({ kind: "monitor", eventCount: 2, droppedLines: 7 });
  });

  it("omits the output recovery hint unless a cursor range is available", () => {
    const timeout = formatMonitorEvent("watch", "mon_1", "[Monitor timed out]", "timeout", {
      output: { suppressedCount: 0 },
    });
    expect(timeout.content).not.toContain("<output");
    const recoverable = formatMonitorEvent("watch", "mon_1", "tick", undefined, {
      output: { cursorStart: 0, cursorEnd: 4 },
    });
    expect(recoverable.content).toContain('cursor-start="0" cursor-end="4"');
  });

  it("escapes a monitor event body, including a fake closing tag", () => {
    const wake = formatMonitorEvent("watch <tests>", "mon_1", "line <a>\n</event>\nline2", "exited");
    expect(wake.content).toContain('<pi-famulus-wake kind="monitor" id="mon_1" description="watch &lt;tests&gt;" status="exited">');
    expect(wake.content).toContain("<event>line &lt;a&gt;\n&lt;/event&gt;\nline2</event>");
    expect(wake.content).not.toContain("<event>line <a>");
    expect(wake.details).toMatchObject({
      kind: "monitor",
      id: "mon_1",
      description: "watch <tests>",
      status: "exited",
      event: "line <a>\n</event>\nline2",
    });
  });

  it("emits subagent-done as one child element per child", () => {
    const wake = formatSubagentNotification({
      runId: "run_a",
      status: "partial",
      durationMs: 100,
      children: [
        { childId: "ch_1", name: "a", status: "completed", text: "ok", prompt: "look" },
        { childId: "ch_2", name: "b", status: "failed", text: "", error: "boom", prompt: "fix" },
      ],
    });
    expect(wake.content).toContain('<pi-famulus-wake kind="subagent-done" run-id="run_a" status="partial" duration-ms="100">');
    expect(wake.content).not.toContain("<subagent-notification>");
    expect(wake.content).toContain('<child id="ch_2" name="b" status="failed">');
    expect(wake.content).toContain("<error>boom</error>");
    expect(wake.content).toContain("<prompt>look</prompt>");
    expect(wake.details).toMatchObject({
      kind: "subagent-done",
      children: [
        { childId: "ch_1", status: "completed", prompt: "look", result: "ok" },
        { childId: "ch_2", status: "failed", error: "boom" },
      ],
    });
  });

  it("puts handover still-running titles in item elements", () => {
    const wake = formatSubagentHandover({
      runId: "run_a",
      childId: "ch_1",
      name: "worker-1",
      status: "completed",
      prompt: "inspect",
      text: "done",
      stillRunning: [{ id: "ch_2", title: "reviewer, slow" }],
    });
    expect(wake.content).toContain('kind="subagent-handover"');
    expect(wake.content).toContain('<item id="ch_2">reviewer, slow</item>');
    expect(wake.content).toContain("<prompt>inspect</prompt>");
    expect(wake.content).toContain("<result>done</result>");
    expect(wake.details).toMatchObject({
      kind: "subagent-handover",
      stillRunning: [{ id: "ch_2", title: "reviewer, slow" }],
    });
  });

  it("puts the reply recipe in reply-with, not after the message", () => {
    const wake = formatSupervisorRequest({ childId: "ch_a", name: "explorer" }, "Which file?");
    expect(wake.content).toContain('<pi-famulus-wake kind="supervisor-request" from="ch_a" name="explorer">');
    expect(wake.content).toContain("<message>Which file?</message>");
    expect(wake.content).toContain(
      '<reply-with>agent_message { action: "reply", to: "ch_a", message: "&lt;your decision&gt;" }</reply-with>',
    );
    const message = wake.content.slice(wake.content.indexOf("<message>"), wake.content.indexOf("</message>"));
    expect(message).not.toContain("action:");
    expect(wake.details).toEqual({
      kind: "supervisor-request",
      from: "ch_a",
      name: "explorer",
      message: "Which file?",
    });
  });
});

describe("pi-famulus-wake pill", () => {
  it("colors from details status and exitCode, not from the summary words", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    expect([...map.keys()]).toEqual(["pi-famulus-wake"]);
    const theme = {
      fg: (_c: string, text: string) => text,
      bg: (_c: string, text: string) => text,
    };
    const render = (details: unknown) =>
      map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
        { content: `${FAMULUS_WAKE_LEAD_IN}\n\n<pi-famulus-wake kind="task">`, details },
        { expanded: false, outputPad: 0 },
        theme,
      ).render(80) as string[];

    const failedSummary = render({
      kind: "task",
      stillRunning: [],
      tasks: [
        {
          id: "sh_1",
          taskKind: "shell",
          status: "completed",
          summary: "Background command completed",
          command: "npm test",
          outputPath: "/tmp/x",
          preview: "",
          durationMs: 1,
          exitCode: 1,
        },
      ],
    });
    expect(failedSummary.join("")).toContain("✗");
    expect(failedSummary.join("")).not.toContain("✓");

    const lyingSummary = render({
      kind: "task",
      stillRunning: [],
      tasks: [
        {
          id: "sh_1",
          taskKind: "shell",
          status: "completed",
          summary: "failed failed failed",
          command: "npm test",
          outputPath: "/tmp/x",
          preview: "",
          durationMs: 1,
          exitCode: 0,
        },
      ],
    });
    expect(lyingSummary.join("")).toContain("✓");
    expect(lyingSummary.join("")).not.toContain("✗");
  });

  it("shows per-status counts and the monitor event, not the lead-in", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const theme = {
      fg: (_c: string, text: string) => text,
      bg: (_c: string, text: string) => text,
    };
    const done = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      {
        content: FAMULUS_WAKE_LEAD_IN,
        details: {
          kind: "subagent-done",
          runId: "run_a",
          status: "partial",
          durationMs: 1,
          summary: "should not be the pill",
          children: [
            { childId: "c1", name: "a", status: "completed", prompt: "", result: "" },
            { childId: "c2", name: "b", status: "completed", prompt: "", result: "" },
            { childId: "c3", name: "c", status: "completed", prompt: "", result: "" },
            { childId: "c4", name: "d", status: "failed", prompt: "", result: "", error: "x" },
          ],
        },
      },
      { expanded: false, outputPad: 0 },
      theme,
    ).render(80) as string[];
    expect(done.join("")).toContain("3 completed · 1 failed");
    expect(done.join("")).not.toContain(FAMULUS_WAKE_LEAD_IN);

    const monitor = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      {
        content: `${FAMULUS_WAKE_LEAD_IN}\nHandle <event> before other work.`,
        details: { kind: "monitor", id: "mon_1", description: "watch tests", event: "line1\nline2", droppedLines: 5, eventCount: 4 },
      },
      { expanded: false, outputPad: 0 },
      theme,
    ).render(80) as string[];
    const text = monitor.join("\n");
    expect(text).toContain("›");
    expect(text).not.toContain("✓");
    expect(text).toContain("line1");
    expect(text).toContain("4 events");
    expect(text).toContain("5 lines dropped");
    expect(text).not.toContain(FAMULUS_WAKE_LEAD_IN);
    expect(text).not.toContain("before other work");
  });

  it("renders expanded wake details as labelled plain text, never XML", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({ registerMessageRenderer(type: string, fn: unknown) { map.set(type, fn as never); } } as never);
    const component = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!({
      content: `${FAMULUS_WAKE_LEAD_IN}\\n\\n<pi-famulus-wake><task id="sh_1" /></pi-famulus-wake>`,
      details: {
        kind: "task",
        stillRunning: [{ id: "sh_2", title: "compile" }],
        tasks: [{ id: "sh_1", taskKind: "shell", status: "completed", summary: "done", command: "make test", outputPath: "/tmp/out", preview: "42 passed", durationMs: 1200, exitCode: 0 }],
      },
    }, { expanded: true, outputPad: 0 }, { fg: (_color: string, text: string) => text, bg: (_color: string, text: string) => text }).render(120) as string[];
    const text = component.join("\\n");
    expect(text).toContain("Tasks (1)");
    expect(text).toContain("$ make test");
    expect(text).toContain("Still running (1): sh_2 compile");
    expect(text).toContain("42 passed");
    expect(text).not.toContain("<pi-famulus-wake");
    expect(text).not.toContain("<task");
  });

  it("shows task duration, exit, still-running count, handover result, and /reply hint", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({ registerMessageRenderer(type: string, fn: unknown) { map.set(type, fn as never); } } as never);
    const theme = { fg: (_color: string, text: string) => text, bg: (_color: string, text: string) => text };
    const render = (details: unknown) => map.get(FAMULUS_WAKE_CUSTOM_TYPE)!({ content: "", details }, { expanded: false, outputPad: 0 }, theme).render(120).join("\\n");
    const task = render({ kind: "task", stillRunning: [{ id: "sh_2", title: "compile" }], tasks: [{ id: "sh_1", taskKind: "shell", status: "completed", summary: "done", command: "true", outputPath: "", preview: "", durationMs: 1200, exitCode: 0 }] });
    expect(task).toContain("1s · exit 0");
    expect(task).toContain("1 still running");
    expect(render({ kind: "subagent-handover", runId: "run_1", childId: "ch_1", name: "alpha", status: "completed", stillRunning: [], summary: "done", prompt: "inspect", result: "Found the root cause" })).toContain("Found the root cause");
    const request = render({ kind: "supervisor-request", from: "ch_1", name: "alpha", message: "Which option?" });
    expect(request).toContain("decision for alpha");
    expect(request).toContain("/reply ch_1 <decision>");
  });

  it("keeps a wide pill inside the terminal width", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const theme = {
      fg: (color: string, text: string) => `\x1b[31m${text}\x1b[0m`,
      bg: (_c: string, text: string) => text,
    };
    const lines = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      {
        content: "x".repeat(400),
        details: {
          kind: "task",
          stillRunning: [],
          tasks: [
            {
              id: "sh_1",
              taskKind: "shell",
              status: "failed",
              summary: `Background command "${"宽".repeat(30)}${"x".repeat(80)}" failed`,
              command: "x",
              outputPath: "/tmp/x",
              preview: "",
              durationMs: 1,
              exitCode: 1,
            },
          ],
        },
      },
      { expanded: false, outputPad: 1 },
      theme,
    ).render(40) as string[];
    expect(lines.length).toBeGreaterThan(0);
    for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(40);
  });
});

describe("monitor-session-end wake", () => {
  const details = {
    kind: "monitor-session-end" as const,
    reason: "new" as const,
    ids: ["mon_a", "mon_b"],
    monitors: [
      { id: "mon_a", description: "alpha logs", elapsedMs: 1500, outputPath: "/tmp/a.output" },
      { id: "mon_b", description: "beta logs", elapsedMs: 2000, outputPath: "/tmp/b.output" },
    ],
  };

  it("is one envelope listing every monitor id, with wakeIds and as-of injection", () => {
    const wake = formatFamulusWake(details);
    expect(wake.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(wake.content.match(/<pi-famulus-wake /g)).toHaveLength(1);
    expect(wake.content).toContain('kind="monitor-session-end"');
    expect(wake.content).toContain("process exits are not confirmed by this notice");
    expect(wake.content).not.toContain("no longer watched");
    expect(wake.content).toContain("mon_a");
    expect(wake.content).toContain("mon_b");
    expect(wake.content).toContain("alpha logs");
    expect(wake.content).toContain("while the log is retained");
    expect(wakeIds(wake.details)).toEqual(["mon_a", "mon_b"]);
    const idsFromMonitors = {
      kind: "monitor-session-end",
      ids: [] as string[],
      monitors: [{ id: "mon_a" }, { id: "mon_b" }],
    };
    expect(wakeIds(idsFromMonitors)).toEqual(["mon_a", "mon_b"]);
    const stamped = stampWakeAsOf(
      { customType: wake.customType, content: wake.content, details: wake.details },
      Date.UTC(2026, 9, 5, 12, 27, 26),
    );
    expect(stamped.content).toContain("as-of=");
    const events: string[] = [];
    const out = onWakeMessageEnd(
      { role: "custom", ...stamped },
      { now: () => Date.UTC(2026, 9, 5, 12, 27, 26) + 4000, logEvent: (type) => events.push(type) },
    );
    expect(events).toEqual(["wake.inject"]);
    expect(out?.content).toContain("age-ms=\"4000\"");
    expect(out?.details).toMatchObject({ kind: "monitor-session-end", ids: ["mon_a", "mon_b"], ageMs: 4000 });
  });

  it("renders a pill without throwing and names the stopped monitors when expanded", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const theme = { fg: (_c: string, text: string) => text, bg: (_c: string, text: string) => text };
    const wake = formatFamulusWake(details);
    const collapsed = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      { content: wake.content, details: wake.details },
      { expanded: false, outputPad: 0 },
      theme,
    ).render(80) as string[];
    const expanded = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      { content: wake.content, details: wake.details },
      { expanded: true, outputPad: 0 },
      theme,
    ).render(80) as string[];
    expect(collapsed.join("")).not.toContain("undefined");
    expect(collapsed.join("")).toContain("2 teardown");
    expect(collapsed.join("")).toContain("new");
    expect(expanded.join("")).toContain("mon_a");
    expect(expanded.join("")).toContain("mon_b");
    expect(expanded.join("")).toContain("while the log is retained");
  });
});

describe("shell exit facts", () => {
  it("retains killed signal and stop source in summary and details", () => {
    const wake = formatTaskNotification([{ taskId: "sh_1", kind: "shell", command: "build", status: "killed", exitCode: null, durationMs: 1000, outputPath: "/tmp/out", preview: "", endReason: "stopped:tui", signal: "SIGTERM" }], []);
    expect(wake.content).toContain("TUI");
    expect(wake.content).toContain("SIGTERM");
    expect(wake.details).toMatchObject({ tasks: [{ endReason: "stopped:tui", signal: "SIGTERM", exitCode: null }] });
    setPiTuiForTests(null);
    const renderers = new Map<string, Function>();
    registerFamulusMessageRenderers({ registerMessageRenderer: (type: string, fn: unknown) => renderers.set(type, fn as never) } as never);
    const theme = { fg: (_c: string, text: string) => text, bg: (_c: string, text: string) => text };
    const expanded = renderers.get(FAMULUS_WAKE_CUSTOM_TYPE)!({ content: wake.content, details: wake.details }, { expanded: true, outputPad: 0 }, theme).render(120).join("\n");
    expect(expanded).toContain("TUI");
    expect(expanded).toContain("SIGTERM");
  });
});

describe("monitor-terminal wake", () => {
  const summaryFirst = `<pi-famulus-wake kind="monitor-terminal">\n  <summary>${MONITOR_UNWATCHED_SUMMARY}</summary>`;
  function assertSummaryFirst(content: string) {
    const xml = content.slice(content.indexOf('<pi-famulus-wake kind="monitor-terminal">'));
    expect(xml.startsWith(summaryFirst)).toBe(true);
  }

  it("is one envelope listing every monitor id, never a single monitor id", () => {
    const wake = formatMonitorTerminal([
      { id: "mon_a", description: "alpha", status: "timeout", event: "Timeout elapsed. Stop requested; exit unconfirmed." },
      { id: "mon_b", description: "beta", status: "timeout" },
    ]);
    expect(wake.customType).toBe(FAMULUS_WAKE_CUSTOM_TYPE);
    expect(wake.content.match(/<pi-famulus-wake /g)).toHaveLength(1);
    expect(wake.content).toContain('kind="monitor-terminal"');
    assertSummaryFirst(wake.content);
    expect(wake.content).not.toMatch(/re-arm if needed/);
    expect(wake.content).toContain("mon_a");
    expect(wake.content).toContain("mon_b");
    expect((wake.details as { id?: string }).id).toBeUndefined();
    expect(wakeIds(wake.details)).toEqual(["mon_a", "mon_b"]);
    const idsFromMonitors = {
      kind: "monitor-terminal",
      ids: [] as string[],
      monitors: [{ id: "mon_a" }, { id: "mon_b" }],
    };
    expect(wakeIds(idsFromMonitors)).toEqual(["mon_a", "mon_b"]);
  });

  it("single-monitor content also starts with the shared unwatched summary", () => {
    const wake = formatMonitorTerminal([{ id: "mon_a", description: "alpha", status: "timeout" }]);
    assertSummaryFirst(wake.content);
    expect(wake.content).toContain("mon_a");
    expect(wake.content).not.toContain("mon_b");
  });

  it("renders mixed confirmed and unconfirmed terminal facts without claiming all stopped", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({ registerMessageRenderer: (type: string, fn: unknown) => map.set(type, fn as never) } as never);
    const theme = { fg: (_c: string, text: string) => text, bg: (_c: string, text: string) => text };
    const wake = formatMonitorTerminal([
      { id: "mon_a", description: "alpha", status: "exited", event: "Stopped via TUI; signal SIGTERM; exit code unknown.", exitFacts: { endReason: "stopped:tui", signal: "SIGTERM", exitCode: null } },
      { id: "mon_b", description: "beta", status: "timeout", event: "Timeout elapsed. Stop requested; exit unconfirmed." },
    ]);
    expect(wake.content).toContain("Stopped via TUI; signal SIGTERM");
    expect(wake.details).toMatchObject({ monitors: [{ exitFacts: { endReason: "stopped:tui", signal: "SIGTERM", exitCode: null } }, {}] });
    const render = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!;
    const collapsed = (render({ content: wake.content, details: wake.details }, { expanded: false, outputPad: 0 }, theme).render(120) as string[]).join("");
    const expanded = (render({ content: wake.content, details: wake.details }, { expanded: true, outputPad: 0 }, theme).render(120) as string[]).join("\n");
    expect(collapsed).not.toMatch(/2 stopped/);
    expect(expanded).toContain("Stopped via TUI; signal SIGTERM");
    expect(expanded).toContain("exit unconfirmed");
  });

  it("renders output metadata in model-visible XML for single and multiple monitors", () => {
    const output = {
      firstObservedAt: Date.UTC(2026, 9, 5, 12, 27, 26),
      lastObservedAt: Date.UTC(2026, 9, 5, 12, 27, 27),
      cursorStart: 10,
      cursorEnd: 40,
      truncated: true,
      suppressedCount: 2,
      omittedChars: 7,
    };
    const single = formatMonitorTerminal([
      { id: "mon_a", description: "alpha", status: "timeout", output },
    ]);
    const multi = formatMonitorTerminal([
      { id: "mon_a", description: "alpha", status: "timeout", output },
      { id: "mon_b", description: "beta", status: "exited", event: "Monitor process exited" },
    ]);
    for (const wake of [single, multi]) {
      assertSummaryFirst(wake.content);
      expect(wake.content).toContain('first-observed-at="2026-10-05T12:27:26Z"');
      expect(wake.content).toContain('last-observed-at="2026-10-05T12:27:27Z"');
      expect(wake.content).toContain('cursor-start="10"');
      expect(wake.content).toContain('cursor-end="40"');
      expect(wake.content).toContain('truncated="true"');
      expect(wake.content).toContain('duplicates-suppressed="2"');
      expect(wake.content).toContain('omitted-chars="7"');
      expect(wake.content).toContain("Read original bytes with task_output");
    }
    expect(multi.content).toContain("mon_b");
  });

  it("renders a pill without throwing and names the stopped monitors when expanded", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const theme = { fg: (_c: string, text: string) => text, bg: (_c: string, text: string) => text };
    const wake = formatMonitorTerminal([
      { id: "mon_a", description: "alpha", status: "timeout" },
      { id: "mon_b", description: "beta", status: "timeout" },
    ]);
    const collapsed = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      { content: wake.content, details: wake.details },
      { expanded: false, outputPad: 0 },
      theme,
    ).render(80) as string[];
    const expanded = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      { content: wake.content, details: wake.details },
      { expanded: true, outputPad: 0 },
      theme,
    ).render(80) as string[];
    expect(collapsed.join("")).not.toContain("undefined");
    expect(collapsed.join("")).toContain("2 lifecycle updates");
    const expandedText = expanded.join("\n");
    expect(expandedText.indexOf(MONITOR_UNWATCHED_SUMMARY)).toBeGreaterThanOrEqual(0);
    expect(expandedText.indexOf(MONITOR_UNWATCHED_SUMMARY)).toBeLessThan(expandedText.indexOf("mon_a"));
    expect(expanded.join("")).toContain("mon_a");
    expect(expanded.join("")).toContain("mon_b");
  });
});
