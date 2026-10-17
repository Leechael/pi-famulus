/**
 * Files and directories the extension creates are private. umask 022 must
 * not leave them group- or world-readable, and a home that already exists
 * as mode 0777 is rejected or tightened rather than reused.
 *
 * POSIX only. Windows ACLs are a different check.
 */
import { chmodSync, mkdirSync, mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { BoundedOutput } from "../../src/bounded-output";
import { ExtensionEventLog } from "../../src/events";
import { writeAgentChildRecord } from "../../src/subagent/agent-records";
import { TranscriptWriter } from "../../src/subagent/transcript";
import { ManualClock } from "../../src/clock";

const POSIX = process.platform !== "win32";

function mode(path: string): number {
  return statSync(path).mode & 0o777;
}

describe.skipIf(!POSIX)("extension filesystem permissions", () => {
  let previousUmask = 0o022;
  const homes: string[] = [];

  beforeAll(() => {
    previousUmask = process.umask(0o022);
  });

  afterAll(() => {
    process.umask(previousUmask);
    for (const home of homes.splice(0)) rmSync(home, { recursive: true, force: true });
  });

  function home(tag: string): string {
    const dir = mkdtempSync(join(tmpdir(), `pi-famulus-mode-${tag}-`));
    homes.push(dir);
    return dir;
  }

  it.fails("creates extension state at 0700/0600", async () => {
    const dir = home("fresh");
    const log = new ExtensionEventLog(dir, () => "sess-1", () => 1);
    log.write("probe", { note: "mode" });
    writeAgentChildRecord(dir, {
      v: 1,
      kind: "agent",
      child_id: "ch_mode",
      run_id: "run_mode",
      session_id: "sess-1",
      name: "mode",
      agent: "worker",
      status: "running",
      started_at: 1,
    });
    const transcript = new TranscriptWriter(dir, new ManualClock());
    const transcriptPath = transcript.sync("sess-1", "ch_mode", [{ role: "user", text: "hello" }]);

    const spill = new BoundedOutput({ maxBytes: 4, maxLines: 100, tailBytes: 4 });
    spill.append(Buffer.from("hello world\n"));
    const spilled = await spill.finish();
    expect(spilled.fullOutputPath).toBeTruthy();
    homes.push(spilled.fullOutputPath!);

    const violations: string[] = [];
    const expectMode = (path: string, want: number) => {
      const got = mode(path);
      if (got !== want) violations.push(`${path} is ${got.toString(8)}, want ${want.toString(8)}`);
    };
    expectMode(join(dir, "sessions"), 0o700);
    expectMode(join(dir, "sessions", "sess-1"), 0o700);
    expectMode(join(dir, "sessions", "sess-1", "agents"), 0o700);
    expectMode(join(dir, "sessions", "sess-1", "events.jsonl"), 0o600);
    expectMode(join(dir, "sessions", "sess-1", "agents", "ch_mode.json"), 0o600);
    expectMode(transcriptPath, 0o600);
    expectMode(spilled.fullOutputPath!, 0o600);
    expect(violations).toEqual([]);
  });

  it.fails("rejects or tightens a pre-existing 0777 state directory", () => {
    const dir = join(tmpdir(), `pi-famulus-mode-open-${process.pid}-${Date.now()}`);
    mkdirSync(dir);
    chmodSync(dir, 0o777);
    homes.push(dir);
    expect(mode(dir)).toBe(0o777);

    let rejected = false;
    try {
      new ExtensionEventLog(dir, () => "sess-1", () => 1).write("probe");
      writeAgentChildRecord(dir, {
        v: 1,
        kind: "agent",
        child_id: "ch_open",
        run_id: "run_open",
        session_id: "sess-1",
        name: "open",
        agent: "worker",
        status: "running",
        started_at: 1,
      });
    } catch {
      rejected = true;
    }
    const tightened = mode(dir) === 0o700;
    expect(rejected || tightened).toBe(true);
  });
});
