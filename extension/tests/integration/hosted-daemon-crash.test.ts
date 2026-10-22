/**
 * When the daemon dies, the hosted child it was running dies with it.
 * The parent must learn that: the child's record must stop saying running,
 * the parent must get a wake that names the child, and the child must be
 * resumable on the restarted daemon.
 *
 * Real daemon, real `pi --mode rpc` child on the fake model, which never
 * answers, so the child is mid-run when the daemon is SIGKILLed.
 *
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/hosted-daemon-crash.test.ts
 */
import { afterEach, describe, expect, it } from "vitest";
import { alive, HOSTED_INTEG, startHostedHarness, waitUntil, type HostedHarness } from "./hosted-harness";

/** Generous: the extension respawns the daemon and reconnects within a few seconds. */
const SETTLE_MS = 20_000;

describe.skipIf(!HOSTED_INTEG)("daemon SIGKILL while a hosted child runs", () => {
  let harness: HostedHarness | null = null;

  afterEach(async () => {
    await harness?.cleanup();
    harness = null;
  });

  it("marks the child interrupted, wakes the parent, and lets it resume", async () => {
    harness = await startHostedHarness({ scenario: "hang" });
    const h = harness;
    const subagent = h.tool("subagent");
    const started = await subagent.execute("sub-1", { async: true, tasks: [{ prompt: "work", name: "worker" }] }, undefined, undefined, h.ctx);
    const runId = (started.details as { run_id: string }).run_id;
    await waitUntil(
      () => h.modelCalls() > 0 && h.agentRecords().some((rec) => rec.status === "running"),
      30_000,
      "the hosted child to be inside its first model call",
    );
    const childId = String(h.agentRecords()[0].child_id);
    const daemon = h.daemonPid();
    const sentBefore = h.sent.length;

    process.kill(daemon, "SIGKILL");
    await waitUntil(() => !alive(daemon), 5_000, "the daemon to exit");

    const wakeNamesChild = () => h.sent.slice(sentBefore).some((sent) => (sent.message.content ?? "").includes(childId));
    const recordStatus = () => String(h.agentRecords().find((rec) => rec.child_id === childId)?.status);
    await waitUntil(() => wakeNamesChild() && recordStatus() !== "running", SETTLE_MS, "a crash wake");
    // Snapshot the crash outcome before resume correctly transitions to pending.
    expect({ recordStatus: recordStatus(), wakeNamesChild: wakeNamesChild() }).toEqual({
      recordStatus: "interrupted", wakeNamesChild: true,
    });

    const callsBeforeResume = h.modelCalls();
    await subagent.execute("sub-2", { action: "resume", run_id: runId, message: "continue" }, undefined, undefined, h.ctx);
    await waitUntil(
      () => h.modelCalls() > callsBeforeResume && recordStatus() === "running",
      30_000,
      "the resumed child to enter another model call",
    );
  }, 90_000);
});
