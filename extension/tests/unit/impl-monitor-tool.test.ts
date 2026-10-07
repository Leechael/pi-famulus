import { describe, expect, it } from "vitest";
import { createMonitorTool, MonitorRegistry } from "../../src/monitor";

describe("monitor tool result rendering", () => {
  it("renders failed tool results as failures and avoids key-hint glyphs", () => {
    const registry = new MonitorRegistry({
      getClient: () => null,
      sessionEnv: () => ({}),
      getNotifyCenter: () => null,
      trackTask: () => {},
    });
    const tool = createMonitorTool(registry);
    const theme = {
      fg: (_color: string, text: string) => text,
      bg: (_color: string, text: string) => text,
    };
    const result = (tool.renderResult as Function)(
      { isError: true, content: [{ type: "text", text: "pi-famulus is not available" }] },
      { expanded: false, isPartial: false },
      theme,
      { isError: true },
    ) as { render(width: number): string[] };
    const text = result.render(120).join("\n");
    expect(text).toContain("✗");
    expect(text).not.toContain("✓");
    expect(text).toContain("manage via /tasks");
    expect(text).not.toContain("↓");
  });
});

// Eval batch 1: gpt-6-luna armed a plain `grep` on the log, which exited at
// once, then stacked three monitors and filled the wait with reads.
describe("monitor tool description", () => {
  it("says the command must keep following its source", () => {
    const registry = new MonitorRegistry({
      getClient: () => null,
      sessionEnv: () => ({}),
      getNotifyCenter: () => null,
      trackTask: () => {},
    });
    const d = createMonitorTool(registry).description;
    expect(d).toMatch(/keep running/);
    expect(d).toMatch(/tail -n \+1 -F/);
    expect(d).toMatch(/simply wait for the next notification/);
    expect(d).toMatch(/Do not call wait_for to wait for monitor events/);
    expect(createMonitorTool(registry).promptGuidelines).toContainEqual(expect.stringContaining("Do not call wait_for"));
  });
});

// Eval batch 4: gpt-6-luna and k3-256k armed a monitor, then checked on it
// once with task_list before waiting. The first "don't check" they saw was
// in that check's result; the monitor's own result said nothing.
describe("monitor start result", () => {
  it("says the event will wake the model and not to check on it", async () => {
    const registry = {
      start: async () => ({ taskId: "mon_1", timeoutMs: 60_000 }),
    } as unknown as MonitorRegistry;
    const res = await createMonitorTool(registry).execute(
      "t",
      { command: "tail -F log | grep -m1 READY", description: "ready" },
      undefined as never,
      undefined as never,
      {} as never,
    );
    const text = (res.content[0] as { text: string }).text;
    expect(text).toMatch(/^Monitor started · task mon_1 · timeout 60s/);
    expect(text).toMatch(/<pi-famulus-wake kind="monitor">/);
    expect(text).toMatch(/no tool call/);
  });

  // Eval 2026-09-30c: with "do not check on it", models still called
  // task_list or task_output once. Claude Code's start result says what will
  // arrive and names the polls.
  it("says what will arrive and names the polls", async () => {
    const registry = {
      start: async () => ({ taskId: "mon_1", timeoutMs: 60_000 }),
    } as unknown as MonitorRegistry;
    const res = await createMonitorTool(registry).execute(
      "t",
      { command: "tail -F log | grep -m1 READY", description: "ready" },
      undefined as never,
      undefined as never,
      {} as never,
    );
    const text = (res.content[0] as { text: string }).text;
    expect(text).toMatch(/when it exits or times out/);
    expect(text).toMatch(/task_list, task_output/);
    expect(text).toMatch(/simply wait for the next notification/);
    expect(text).toMatch(/Do not call wait_for to wait for monitor events/);
    expect(text).toMatch(/re-arming a monitor/);
  });

  it("keeps the transcript row to the first line", () => {
    const registry = new MonitorRegistry({ getClient: () => null, sessionEnv: () => ({}), getNotifyCenter: () => null, trackTask: () => {} });
    const theme = { fg: (_c: string, t: string) => t, bg: (_c: string, t: string) => t };
    const row = (createMonitorTool(registry).renderResult as Function)(
      { content: [{ type: "text", text: "Monitor started · task mon_1 · timeout 60s\nEvents arrive as …" }] },
      { expanded: false, isPartial: false },
      theme,
      { isError: false },
    ) as { render(width: number): string[] };
    expect(row.render(200).join("\n")).not.toContain("Events arrive");
  });
});
