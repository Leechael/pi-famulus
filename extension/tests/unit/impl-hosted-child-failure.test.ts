/**
 * I-37. A hosted child that fails must settle as failed, and the parent's
 * wake must carry the failure. Two ways a child fails:
 *
 * - the provider rejects the request (429 rate limit, 500 server error):
 *   pi ends the run with an assistant message whose stopReason is "error"
 *   and whose errorMessage is the provider's text;
 * - the child process dies: the daemon reports activity "failed"
 *   (manager/src/hosted.rs on_exit, nonzero or signal exit).
 *
 * The daemon is a fake that sends exactly those records; the extension,
 * HostedRpcSession and the runner are the production code.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { resetHostedFakes, startHostedExtension, until, wakeDetails } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-hosted-fail-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));

type DoneChild = { status?: string; error?: string };

async function settledChild(fail: (manager: Awaited<ReturnType<typeof startHostedExtension>>["manager"], childId: string) => void) {
  const ext = await startHostedExtension({ sessionId: `sess-${Math.random().toString(36).slice(2)}`, cwd: home });
  try {
    const childId = await ext.spawnChild();
    fail(ext.manager, childId);
    await until(() => wakeDetails(ext.pi.sent, "subagent-done").length > 0, "the subagent-done wake");
    const wake = ext.pi.sent.find((entry) => (entry.message.details as { kind?: string })?.kind === "subagent-done")!;
    const child = ((wake.message.details as { children?: DoneChild[] }).children ?? [])[0] ?? {};
    return { child, content: wake.message.content ?? "" };
  } finally {
    await ext.shutdown();
  }
}

afterEach(() => resetHostedFakes());

describe("I-37 a failed hosted child is reported as failed", () => {
  it.each([
    ["429 rate limit", "429 Too Many Requests: rate limit exceeded, retry after 20s"],
    ["500 server error", "500 Internal Server Error: upstream overloaded"],
  ])("provider error (%s) settles failed with the provider's text in the wake", async (_label, providerText) => {
    const { child, content } = await settledChild((manager, childId) => {
      manager.finish(childId, { content: [], stopReason: "error", errorMessage: providerText });
    });
    expect({
      status: child.status,
      error: child.error,
      wakeHasProviderText: content.includes(providerText),
    }).toEqual({
      status: "failed",
      error: expect.stringContaining(providerText),
      wakeHasProviderText: true,
    });
  });

  it("a child process that dies mid-run settles failed with an error, not completed", async () => {
    const { child } = await settledChild((manager, childId) => {
      manager.rpc(childId, { type: "agent_start" });
      manager.state(childId, "failed", "none");
    });
    expect({ status: child.status, hasError: Boolean(child.error) }).toEqual({ status: "failed", hasError: true });
  });
});
