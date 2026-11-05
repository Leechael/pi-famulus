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
    /Expiry requests this monitor's stop; the timeout notice can arrive late while you are busy/;
  const sampling =
    /Sampling windows must not overlap and must not leave gaps \(window = interval\), or keep a cursor\/last-seen state in the script and report only new items/;
  const stagger = /If you use timeouts for several monitors, stagger them/;

  it("description carries lifecycle once and keeps required behavior", () => {
    const description = createMonitorTool(emptyRegistry()).description;
    expect(description).toMatch(/timeout_ms defaults to 5 minutes \(max 1 hour\)/);
    expect(description).toMatch(expiry);
    expect(description).toMatch(/persistent:true for long monitoring/);
    expect(description).toMatch(stagger);
    expect(description).toMatch(/Prefer `tail -F` \/ `journalctl -f` over polling/);
    expect(description).toMatch(/edge-triggered/);
    expect(description).toMatch(/normally zero output, one line only for a new problem signature/);
    expect(description).toMatch(/one recovery line/);
    expect(description).toMatch(sampling);
    expect(description).toMatch(/do not print a heartbeat on every check/);
    expect(description).toMatch(/keep running/);
    expect(description).toMatch(/Silence is not success/);
    expect(description).toMatch(/Do not poll/);
    expect(description).toMatch(/simply wait for the next notification/);
    expect(description).not.toMatch(/expiry is a silent event/);
    expect(description).not.toMatch(/liveness belongs/);
    expect(description).not.toMatch(/same instant/);
    expect(description).not.toMatch(/self-heartbeats/);
  });

  it("rules only add the short persistent hint", () => {
    const guidelines = createMonitorTool(emptyRegistry()).promptGuidelines ?? [];
    const rules = guidelines.join("\n");
    expect(guidelines.at(-1)).toBe("Use persistent:true for long monitoring.");
    expect(guidelines).toHaveLength(4);
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
    expect(params.properties.timeout_ms.description).toMatch(/5 minutes/);
    expect(params.properties.timeout_ms.description).toMatch(/1 hour/);
    expect(params.properties.timeout_ms.description).toMatch(/default/);
    expect(params.properties.timeout_ms.description).not.toMatch(/silent event/);
    expect(params.properties.timeout_ms.description).not.toMatch(/Expiry ends/);
    expect(params.properties.persistent.description).toMatch(/persistent:true for long monitoring/);
  });

  it("docs\/tools.md and README carry the same monitor lifecycle guidance", () => {
    const tools = readFileSync(join(repoRoot, "docs", "tools.md"), "utf8");
    const readme = readFileSync(join(repoRoot, "README.md"), "utf8");
    expect(tools).toMatch(/Exit and timeout produce notifications/);
    for (const text of [tools, readme]) {
      expect(text).toMatch(/5 minutes/);
      expect(text).toMatch(/1 hour/);
      expect(text).toMatch(expiry);
      expect(text).toMatch(/persistent:true/);
      expect(text).toMatch(stagger);
      expect(text).toMatch(/journalctl -f/);
      expect(text).toMatch(/edge-triggered/);
      expect(text).toMatch(sampling);
      expect(text).not.toMatch(/expiry is a silent event/);
      expect(text).not.toMatch(/liveness belongs/);
      expect(text).not.toMatch(/same instant/);
    }
  });
});
