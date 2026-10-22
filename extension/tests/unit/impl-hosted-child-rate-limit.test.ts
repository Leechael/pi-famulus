/**
 * A hosted child that hits a provider rate limit must settle as
 * failed with endReason rate-limit, and the record/wake must carry
 * retry-after when the provider named it. The child is no longer
 * reported as completed.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { agentRecordPath } from "../../src/subagent/agent-records";
import { resetHostedFakes, startHostedExtension, until, wakeDetails } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-rate-limit-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("a rate-limited hosted child is recorded distinctly", () => {
  it("records rate-limit and retry-after on the wake and the child record", async () => {
    const providerText = "429 Too Many Requests: rate limit exceeded, retry after 20s";
    const ext = await startHostedExtension({ sessionId: "sess-rate", cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.finish(childId, { content: [], stopReason: "error", errorMessage: providerText });
      await until(() => wakeDetails(ext.pi.sent, "subagent-done").length > 0, "the subagent-done wake");
      const wake = ext.pi.sent.find((entry) => (entry.message.details as { kind?: string })?.kind === "subagent-done")!;
      const child = ((wake.message.details as { children?: Array<{ endReason?: string; retryAfter?: number; error?: string }> }).children ?? [])[0] ?? {};
      const record = JSON.parse(readFileSync(agentRecordPath(home, "sess-rate", childId), "utf8")) as {
        end_reason?: string;
        retry_after?: number;
      };
      expect({
        wakeEndReason: child.endReason,
        wakeRetryAfter: child.retryAfter,
        wakeHasProviderText: (child.error ?? "").includes(providerText),
        recordEndReason: record.end_reason,
        recordRetryAfter: record.retry_after,
      }).toEqual({
        wakeEndReason: "rate-limit",
        wakeRetryAfter: 20,
        wakeHasProviderText: true,
        recordEndReason: "rate-limit",
        recordRetryAfter: 20,
      });
    } finally {
      await ext.shutdown();
    }
  });
});
