/**
 * I-43. session_start connects to the daemon in the background. A subagent
 * call that lands before that connect finishes must wait for it (bounded),
 * not fail at once.
 *
 * The fake daemon holds connect() until the test releases it, so the call
 * always lands first. The child is released right after the call returns.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { hostedFakeState, resetHostedFakes, settleAsync, startHostedExtension, until, wakeDetails } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-cold-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("I-43 a subagent call made before the daemon connects", () => {
  it.fails("waits for the connection and then runs the child", async () => {
    let release!: () => void;
    hostedFakeState().connectGate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const ext = await startHostedExtension({ sessionId: "sess-cold", cwd: home });
    try {
      expect(ext.manager.isAvailable()).toBe(false);
      await ext.run({ async: true, tasks: [{ prompt: "work", name: "worker" }] });
      await settleAsync();
      release();
      await until(() => ext.manager.spawns.length > 0, "the hosted child to be spawned", 1000).catch(() => {});
      const spawned = ext.manager.spawns.length;
      if (spawned > 0) {
        const childId = ext.manager.spawns[0].childId;
        await until(() => ext.manager.inputs.some((input) => input.op === "prompt"), "the child prompt");
        ext.manager.finish(childId, { content: [{ type: "text", text: "child finished" }], stopReason: "stop" });
      }
      await until(() => wakeDetails(ext.pi.sent, "subagent-done").length > 0, "the subagent-done wake");
      const child = ((wakeDetails(ext.pi.sent, "subagent-done")[0].children ?? []) as Array<{ status?: string; error?: string }>)[0];
      expect({ spawned, status: child?.status, error: child?.error }).toEqual({ spawned: 1, status: "completed", error: undefined });
    } finally {
      release();
      await ext.shutdown();
    }
  });
});
