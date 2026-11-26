import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { createMonitorTool, MonitorRegistry } from "../../src/monitor";
import { WorkIndex } from "../../src/work-index";

const repoRoot = join(import.meta.dirname, "..", "..", "..");

function emptyRegistry() {
  return new MonitorRegistry({
    getClient: () => null,
    sessionEnv: () => ({}),
    getNotifyCenter: () => null,
    trackTask: () => {},
  });
}

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
    expect(text).toContain("· /tasks");
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

describe("monitor transcript row status", () => {
  const registry = () => new MonitorRegistry({ getClient: () => null, sessionEnv: () => ({}), getNotifyCenter: () => null, trackTask: () => {} });
  const theme = { fg: (_c: string, t: string) => t, bg: (_c: string, t: string) => t };
  const render = (getIndex: () => WorkIndex | null) =>
    ((createMonitorTool(registry(), getIndex).renderResult as Function)(
      { content: [{ type: "text", text: "Monitor started · task mon_1 · timeout 60s" }], details: { task_id: "mon_1", timeout_ms: 60_000 } },
      { expanded: false, isPartial: false },
      theme,
      { isError: false, state: {} },
    ) as { render(width: number): string[] }).render(200).join("\n");

  it("shows a monitor the index no longer holds as finished, not running", () => {
    expect(render(() => new WorkIndex())).toContain("✓");
    expect(render(() => new WorkIndex())).not.toContain("●");
  });

  it("shows an indexed live monitor as running", () => {
    const index = new WorkIndex();
    index.upsert({ id: "mon_1", kind: "monitor", status: "running", title: "ready", startedAt: 0, countsAsWorker: false });
    expect(render(() => index)).toContain("●");
  });
});

describe("monitor lifecycle guidance", () => {
  const expiry =
    /expiry stops the monitor and the notice can arrive late while you are busy/;
  const sampling =
    /Sampling windows must not overlap and must not leave gaps \(window = interval\), or keep a cursor\/last-seen state in the script and report only new items/;
  const stagger = /If you use timeouts for several monitors, stagger them/;

  it("description carries lifecycle once and keeps required behavior", () => {
    const description = createMonitorTool(emptyRegistry()).description;
    expect(description).toMatch(/A monitor runs until its command exits or you stop it with task_stop: it has no time limit unless you set one/);
    expect(description).toMatch(/Set timeout_ms \(max 1 hour\) only when the user asked for a time-bounded watch/);
    expect(description).toMatch(expiry);
    expect(description).toMatch(/do not re-arm monitors/);
    expect(description).toMatch(/Do not size a timeout to how long you expect to wait/);
    expect(description).toMatch(/Never set timeout_ms with persistent:true/);
    expect(description).not.toMatch(/default 5 minutes/);
    expect(description).not.toMatch(/Use persistent:true when/);
    expect(description).toMatch(/make it the command/);
    expect(description).toMatch(/do not add end-of-watch lines/);
    expect(description).toMatch(stagger);
    expect(description).toMatch(/`journalctl -f` for a source you did not start/);
    expect(description).toMatch(/edge-triggered/);
    expect(description).toMatch(/normally zero output, one line only for a new problem signature/);
    expect(description).toMatch(/one recovery line/);
    expect(description).toMatch(sampling);
    expect(description).toMatch(/do not print a heartbeat on every check/);
    expect(description).toMatch(/without wake_on, do not print a heartbeat on every check/);
    expect(description).toMatch(/Without wake_on, write edge-triggered commands: normally zero output/);
    expect(description).toMatch(/set wake_on to the problem pattern: only matching lines wake you, and task_output keeps every line/);
    expect(description).toMatch(/Use stall_after_ms instead of heartbeats/);
    expect(description).toMatch(/keep running/);
    expect(description).toMatch(/Silence is not success/);
    expect(description).toMatch(/Do not poll/);
    expect(description).toMatch(/simply wait for the next notification/);
    expect(description).not.toMatch(/expiry is a silent event/);
    expect(description).not.toMatch(/liveness belongs/);
    expect(description).not.toMatch(/same instant/);
    expect(description).not.toMatch(/self-heartbeats/);
  });

  it("rules only add the short lifetime hint", () => {
    const guidelines = createMonitorTool(emptyRegistry()).promptGuidelines ?? [];
    const rules = guidelines.join("\n");
    expect(guidelines[3]).toBe("A monitor runs until its command exits or you stop it; when you no longer need it, stop it with task_stop. Set timeout_ms only when the user asked for a time limit.");
    expect(guidelines.at(-1)).toBe("For a monitor that stays up, set wake_on to the problem pattern so only problems wake you, and stall_after_ms instead of heartbeats.");
    expect(rules).toMatch(/Report from the <event> itself/);
    expect(guidelines).toHaveLength(5);
    expect(rules).toMatch(/Use the monitor tool to watch for conditions/);
    expect(rules).toMatch(/<pi-famulus-wake kind="monitor">/);
    expect(rules).not.toMatch(expiry);
    expect(rules).not.toMatch(stagger);
    expect(rules).not.toMatch(/edge-triggered/);
    expect(rules).not.toMatch(sampling);
    expect(rules).not.toMatch(/liveness belongs/);
    expect(rules).not.toMatch(/expiry is a silent event/);
  });

  it("params carry timeout bounds and persistent long-monitoring only", () => {
    const params = JSON.parse(JSON.stringify(createMonitorTool(emptyRegistry()).parameters)) as {
      properties: { timeout_ms: { description: string }; persistent: { description: string } };
    };
    expect(params.properties.timeout_ms.description).toMatch(/Only when the user asked for a time-bounded watch/);
    expect(params.properties.timeout_ms.description).toMatch(/1 hour/);
    expect(params.properties.timeout_ms.description).toMatch(/persistent:false without it stops after 300000 ms/);
    expect(params.properties.timeout_ms.description).toMatch(/runs until its command exits or you stop it/);
    expect(params.properties.timeout_ms.description).not.toMatch(/silent event/);
    expect(params.properties.timeout_ms.description).not.toMatch(/Expiry ends/);
    expect(params.properties.persistent.description).toMatch(/Not needed: a monitor without timeout_ms already runs until its command exits/);
    expect(params.properties.persistent.description).toMatch(/persistent:false applies the 5-minute default/);
    expect(params.properties.persistent.description).toMatch(/Exclusive with timeout_ms/);
    expect(params.properties.timeout_ms.description).toMatch(/Exclusive with persistent/);
  });

  it("params describe wake_on and stall_after_ms", () => {
    const params = JSON.parse(JSON.stringify(createMonitorTool(emptyRegistry()).parameters)) as {
      properties: { wake_on: { description: string }; stall_after_ms: { description: string } };
    };
    expect(params.properties.wake_on.description).toMatch(/only matching lines wake you/);
    expect(params.properties.wake_on.description).toMatch(/every line is still recorded for task_output/);
    expect(params.properties.wake_on.description).toMatch(/Strongly recommended for persistent monitors/);
    expect(params.properties.stall_after_ms.description).toMatch(/min 10000, max 86400000/);
    expect(params.properties.stall_after_ms.description).toMatch(/instead of heartbeat lines/);
  });

  it("docs\/tools.md and README carry the same monitor lifecycle guidance", () => {
    const tools = readFileSync(join(repoRoot, "docs", "tools.md"), "utf8");
    const readme = readFileSync(join(repoRoot, "README.md"), "utf8");
    expect(tools).toMatch(/Exit and timeout produce notifications/);
    for (const text of [tools, readme]) {
      expect(text).toMatch(/1 hour/);
      expect(text).toMatch(/task_stop/);
      expect(text).toMatch(/neither `timeout_ms` nor `persistent`|no time limit|until its command exits/);
      expect(text).toMatch(expiry);
      expect(text).toMatch(/persistent:false/);
      expect(text).not.toMatch(/default 5 minutes|defaults to 300000/);
      expect(text).toMatch(stagger);
      expect(text).toMatch(/journalctl -f/);
      expect(text).toMatch(/edge-triggered/);
      expect(text).toMatch(sampling);
      expect(text).toMatch(/set `wake_on` to the problem pattern: only matching lines wake the agent, and `task_output` keeps every line/);
      expect(text).toMatch(/`stall_after_ms` instead of heartbeats/);
      expect(text).toMatch(/[Ww]ithout `wake_on`, (write|commands should be) edge-triggered/);
      expect(text).not.toMatch(/expiry is a silent event/);
      expect(text).not.toMatch(/liveness belongs/);
      expect(text).not.toMatch(/same instant/);
    }
  });
});
