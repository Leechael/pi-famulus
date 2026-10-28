/**
 * A hosted child must not write the user's global settings.
 * Sending set_steering_mode makes pi persist steeringMode=all into
 * settings.json. Hosted children skip that RPC.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { resetHostedFakes, startHostedExtension } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-steering-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("hosted children do not write global settings", () => {
  it("does not send set_steering_mode when spawning a hosted child", async () => {
    const ext = await startHostedExtension({ sessionId: "sess-steer", cwd: home });
    try {
      await ext.spawnChild();
      expect(ext.manager.inputs.some((input) => input.op === "set_steering_mode")).toBe(false);
    } finally {
      await ext.shutdown();
    }
  });
});
