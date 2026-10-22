/**
 * I-17. Large foreground bash output spills to
 * tmpdir/pi-famulus-bash-<id>.log and the result text names that path.
 * The file must stay while the session still refers to it, and must be
 * gone after session shutdown. The second assertion fails today: nothing
 * unlinks the file, and the daemon's 24h GC does not cover tmpdir.
 */
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { hostedFakeState, resetHostedFakes, startHostedExtension, textOf } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = join(tmpdir(), `pi-famulus-spill-${process.pid}`);
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);

const SPILL_PREFIX = "pi-famulus-bash-";
const leftover: string[] = [];

function pathFromResult(text: string): string | undefined {
  const match = text.match(/Full output: ([^\s\]]+)/);
  return match?.[1];
}

beforeEach(() => {
  resetHostedFakes();
  hostedFakeState().available = false;
  mkdirSync(home, { recursive: true });
});

afterEach(() => {
  resetHostedFakes();
  rmSync(home, { recursive: true, force: true });
  for (const path of leftover.splice(0)) {
    rmSync(path, { force: true });
  }
});

async function overflowForeground(): Promise<{
  text: string;
  path: string;
  shutdown: () => Promise<void>;
}> {
  const ext = await startHostedExtension({ sessionId: "sess-spill", cwd: tmpdir() });
  const result = await ext.pi.tool("bash").execute(
    "bash-spill",
    { command: "seq 1 3000" },
    undefined,
    undefined,
    ext.ctx,
  );
  const text = textOf(result);
  const path = pathFromResult(text);
  if (!path) throw new Error(`result did not name a spill file:\n${text}`);
  leftover.push(path);
  return { text, path, shutdown: ext.shutdown };
}

describe("I-17 foreground bash overflow file", () => {
  it("keeps the spill file while the result text names it", async () => {
    const ran = await overflowForeground();
    try {
      expect(ran.path).toMatch(new RegExp(`${SPILL_PREFIX}[0-9a-f-]+\\.log$`));
      expect(ran.text).toContain(ran.path);
      expect(existsSync(ran.path)).toBe(true);
    } finally {
      await ran.shutdown();
    }
  });

  it.fails("deletes the spill file after session shutdown", async () => {
    const ran = await overflowForeground();
    expect(existsSync(ran.path)).toBe(true);
    await ran.shutdown();
    expect(existsSync(ran.path)).toBe(false);
  });
});
