/**
 * Real daemon + real RPC pi + local faux model: a parked child that is resumed
 * must stay supervised by its parent until the actual turn ends.
 *
 * Incident sequence (2026-11-15, ch_2386fec0): interrupt -> parked ->
 * `agent_message send` -> the daemon respawns the child and its cold
 * `get_state` handshake reports idle before the resumed prompt runs. The
 * parent used to settle the new generation on that idle.
 *
 * The faux `hang` scenario starts a model request and never answers, so a
 * parent record that is no longer `running` while the second model call is
 * executing means the resumed turn was abandoned.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { HOSTED_INTEG, startHostedHarness, waitUntil, type HostedHarness } from "./hosted-harness";

/** Terminal parent wakes (`subagent-done`), in send order. */
function terminalWakes(h: HostedHarness): Array<Record<string, unknown>> {
  return h.sent
    .map((entry) => entry.message.details as Record<string, unknown> | undefined)
    .filter((details): details is Record<string, unknown> => details?.kind === "subagent-done");
}

describe.skipIf(!HOSTED_INTEG)("parked child resume supervision", () => {
  let h: HostedHarness | undefined;
  afterEach(async () => {
    await h?.cleanup();
  });

  it("keeps the resumed turn running until the child settles", async () => {
    h = await startHostedHarness({ scenario: "hang" });
    await h.tool("subagent").execute("start", { async: true, tasks: [{ prompt: "work", name: "resume-supervision" }] }, undefined, undefined, h.ctx);
    await waitUntil(() => h!.modelCalls() >= 1, 30000, "first model call");
    const id = String(h.agentRecords()[0].child_id);
    const run = (params: Record<string, unknown>) => h!.tool("subagent").execute("action", params, undefined, undefined, h!.ctx);
    await run({ action: "interrupt", child_id: id });
    // Force the same parked state as the incident's two-minute retention,
    // without waiting two minutes or changing global daemon configuration.
    await run({ action: "interrupt", child_id: id });
    await waitUntil(
      () => JSON.parse(readFileSync(join(h!.home, "agents", `${id}.json`), "utf8")).residency === "parked",
      5000,
      "child parked",
    );
    const terminalWakesBefore = terminalWakes(h).length;
    await h.tool("agent_message").execute("send", { action: "send", to: id, message: "Resume. New scope: PR B" }, undefined, undefined, h.ctx);
    await waitUntil(() => h!.modelCalls() >= 2, 30000, "resumed child executing model request");

    const record = h.agentRecords().find((r) => r.child_id === id)!;
    const after = terminalWakes(h).slice(terminalWakesBefore);
    expect({
      status: record.status,
      modelCalls: h.modelCalls(),
      terminalWakes: after,
    }).toEqual({
      status: "running",
      modelCalls: 2,
      terminalWakes: [],
    });
  }, 60000);
});
