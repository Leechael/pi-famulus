/**
 * Print mode stops background work and still exits 0. stderr must say so.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { printModeDropWarning } from "../../src/work-index";
import { resetHostedFakes, startHostedExtension } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-print-drop-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("print mode drop warning", () => {
  it("names the work that print mode is about to stop", () => {
    expect(printModeDropWarning([])).toBeNull();
    expect(printModeDropWarning(["ch_1", "sh_2"])).toBe(
      "pi-famulus: print mode is ending; stopping 2 background item(s) (ch_1, sh_2). The process still exits 0.\n",
    );
  });

  it("writes the warning when print mode ends with a hosted child still running", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-print", cwd: home, mode: "print" });
    const childId = await ext.spawnChild();
    const writes: string[] = [];
    const orig = process.stderr.write.bind(process.stderr);
    process.stderr.write = ((chunk: unknown, ...rest: unknown[]) => {
      writes.push(String(chunk));
      return (orig as (chunk: unknown, ...rest: unknown[]) => boolean)(chunk, ...rest);
    }) as typeof process.stderr.write;
    try {
      await ext.shutdown();
    } finally {
      process.stderr.write = orig;
    }
    expect(writes.join("")).toContain("print mode is ending");
    expect(writes.join("")).toContain(childId);
  });
});
