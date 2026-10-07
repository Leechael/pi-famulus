/**
 * Every text a model can see from this extension, rendered with fixed inputs:
 * tool declarations (description, rules, parameter schema), behavior
 * guidelines, builtin agents, and wake texts. The snapshot is the contract
 * that moving prompts into extension/prompts/ changes no model-visible byte.
 */
import { describe, expect, it } from "vitest";
import { BUILTIN_AGENTS } from "../../src/agents/builtins";
import { BEHAVIOR_GUIDELINES, CHILD_BEHAVIOR_GUIDELINES } from "../../src/behavior-guidelines";
import { createBashOverride } from "../../src/bash-override";
import { createAgentMessageTool, createContactSupervisorTool } from "../../src/comms/tools";
import {
  formatBackgroundNotice,
  formatMonitorEvent,
  formatSubagentHandover,
  formatSubagentNotification,
  formatSubagentOverrun,
  formatSubagentOverrunBatch,
  formatTaskNotification,
} from "../../src/format";
import { createMonitorTool, MONITOR_STARTED_INSTRUCTION } from "../../src/monitor";
import { createChildBashTool } from "../../src/subagent/child-bash";
import { createSubagentTool } from "../../src/subagent/tool";
import { createTaskListTool, createTaskOutputTool, createTaskStopTool } from "../../src/task-tools";
import { FAMULUS_WAKE_LEAD_IN, formatFamulusWake } from "../../src/wake";

const stub = {} as never;

function declaration(tool: { name: string; description: string; promptSnippet?: string; promptGuidelines?: string[]; parameters: unknown }) {
  return {
    name: tool.name,
    description: tool.description,
    promptSnippet: tool.promptSnippet,
    promptGuidelines: tool.promptGuidelines,
    parameters: JSON.parse(JSON.stringify(tool.parameters)),
  };
}

describe("model-visible prompt surface", () => {
  it("tool declarations", () => {
    const tools = [
      createBashOverride(stub),
      createTaskListTool(stub),
      createTaskOutputTool(stub),
      createTaskStopTool(stub),
      createMonitorTool(stub),
      createSubagentTool({ defaultTimeoutMs: 1_800_000 } as never),
      createAgentMessageTool(stub, { kind: "parent" }, stub),
      createAgentMessageTool(stub, { kind: "child", childId: "ch_1" } as never, stub),
      createContactSupervisorTool(stub, "ch_1"),
      createChildBashTool(stub),
    ];
    expect(tools.map(declaration)).toMatchSnapshot();
  });

  it("guidelines and builtin agents", () => {
    expect({ BEHAVIOR_GUIDELINES, CHILD_BEHAVIOR_GUIDELINES, BUILTIN_AGENTS }).toMatchSnapshot();
  });

  it("tool result instructions", () => {
    expect({
      background: formatBackgroundNotice("sh_00000001", "./build.sh", "/tmp/out/sh_00000001.output"),
      monitorStarted: MONITOR_STARTED_INSTRUCTION,
    }).toMatchSnapshot();
  });

  it("wakes", () => {
    const asOf = { asOf: Date.UTC(2026, 9, 7, 12, 0, 0) };
    const task = formatTaskNotification([
      { taskId: "sh_1", kind: "shell", command: "./build.sh", status: "completed", exitCode: 0, durationMs: 15_000, outputPath: "/tmp/o", preview: "BUILD OK" },
    ]);
    const monitor = formatMonitorEvent("READY in service.log", "mon_1", "READY token=X\n");
    const done = formatSubagentNotification({
      runId: "run_1",
      status: "completed",
      durationMs: 9000,
      children: [{ childId: "ch_1", name: "a", status: "completed", prompt: "p", text: "r" } as never],
    });
    const handover = formatSubagentHandover({
      runId: "run_1", childId: "ch_1", name: "a", status: "completed", prompt: "p", text: "r", stillRunning: [{ id: "ch_2", title: "b" }],
    });
    const overrunInfo = {
      runId: "run_1", childId: "ch_1", name: "a", elapsedMs: 60_000, budgetMs: 60_000, reminder: 1, nextReminderMs: 600_000,
      lastActivity: { agoMs: 50_000, text: "assistant: tool bash" },
      shell: { taskId: "sh_1", elapsedMs: 55_000, command: "./x.sh", outputPath: "/tmp/o", outputBytes: 10, outputIdleMs: 50_000, growing: false },
    };
    const overrun = formatSubagentOverrun(overrunInfo);
    const overrunCeiling = formatSubagentOverrun({ ...overrunInfo, hardCeilingMs: 30_000 });
    const overrunBatch = formatSubagentOverrunBatch([overrunInfo, { ...overrunInfo, childId: "ch_2", name: "b" }]);
    const request = formatFamulusWake({ kind: "supervisor-request", from: "ch_1", name: "a", message: "JSON or YAML?" });
    const update = formatFamulusWake({ kind: "supervisor-update", from: "ch_1", name: "a", message: "halfway" });
    const changed = formatFamulusWake({
      ...(done.details as object),
      ...asOf,
      runStatusNow: "running",
      children: [{ childId: "ch_1", name: "a", status: "running", statusAsOf: "completed", prompt: "p", result: "r" }],
    } as never);
    expect({
      leadIn: FAMULUS_WAKE_LEAD_IN,
      task: task.content,
      monitor: monitor.content,
      done: done.content,
      handover: handover.content,
      overrun: overrun.content,
      overrunCeiling: overrunCeiling.content,
      overrunBatch: overrunBatch.content,
      request: request.content,
      update: update.content,
      changed: changed.content,
    }).toMatchSnapshot();
  });
});
