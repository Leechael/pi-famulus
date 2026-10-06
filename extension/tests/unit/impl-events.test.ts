import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { createExtensionEventLog, formatExtensionEventLine } from "../../src/events";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("extension event log", () => {
  it("appends timestamped extension events to the session events.jsonl", () => {
    const home = mkdtempSync(join(tmpdir(), "pi-famulus-events-"));
    dirs.push(home);
    const clock = new ManualClock(1234);
    const log = createExtensionEventLog(home, () => "session-a", clock);
    log.write("wake.emit", { kind: "task", ids: ["sh_1"], batch: false });
    const file = join(home, "sessions", "session-a", "events.jsonl");
    const rows = readFileSync(file, "utf8").trim().split("\n").map((row) => JSON.parse(row));
    expect(rows).toEqual([{ ts: 1234, src: "extension", type: "wake.emit", kind: "task", ids: ["sh_1"], batch: false }]);
  });

  it("writes absolute subagent token usage for manager-side readers", () => {
    const home = mkdtempSync(join(tmpdir(), "pi-famulus-events-"));
    dirs.push(home);
    const log = createExtensionEventLog(home, () => "session-usage", new ManualClock(2000));
    log.write("agent.usage", {
      child_id: "ch_1",
      tokens_input: 480,
      tokens_output: 32,
      llm_ms: 1_200,
      tool_ms: 300,
      queue_ms: 50,
      wall_other_ms: 25,
      wall_approximate: true,
    });
    const file = join(home, "sessions", "session-usage", "events.jsonl");
    const row = JSON.parse(readFileSync(file, "utf8").trim());
    expect(row).toMatchObject({
      src: "extension",
      type: "agent.usage",
      child_id: "ch_1",
      tokens_input: 480,
      tokens_output: 32,
      llm_ms: 1_200,
      tool_ms: 300,
      queue_ms: 50,
      wall_other_ms: 25,
      wall_approximate: true,
    });
  });

  it("skips events without a session and caps each serialized row below 4 KiB", () => {
    const home = mkdtempSync(join(tmpdir(), "pi-famulus-events-"));
    dirs.push(home);
    createExtensionEventLog(home, () => "", new ManualClock()).write("agent.settle");
    expect(existsSync(join(home, "sessions"))).toBe(false);
    const row = formatExtensionEventLine({ ts: 1, src: "extension", type: "agent.settle", error: "x".repeat(20_000) });
    expect(Buffer.byteLength(row, "utf8")).toBeLessThan(4096);
    expect(JSON.parse(row).truncated).toBe(true);
  });
});
