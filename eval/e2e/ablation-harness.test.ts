/**
 * Self-test for the ablation harness (faux model, no cost). Guards the
 * manifest against drift and proves the harness is not a placebo:
 *  1. baseline: every reachable segment IS in what the model receives
 *     (a reworded prompt in extension/src fails here);
 *  2. everything ablated: no segment is left anywhere the model sees, the
 *     audit log recorded a hit for each, and the wakes still arrive;
 *  3. mech.sleep-block: the bare-sleep rejection fires only in baseline.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import { compileTextSegments, loadManifest } from "../ablation/manifest.ts";
import { ABLATION_EXT } from "../lib/paths.ts";
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import type { WakeKind } from "../lib/wake-adapter.ts";
import { type FauxEpisode, runFaux } from "./run-faux.ts";

const manifest = loadManifest();
const textSegments = manifest.segments.filter((s) => s.kind === "text" || s.kind === "regex");
/** Removable by the harness: what the parent model sees. */
const reachable = textSegments.filter((s) => s.ablatable !== false);
/** Child-only texts: drift-guarded in child calls, never ablated. */
const childOnly = textSegments.filter((s) => s.scope === "child");
/** Every ablatable text segment at once (mechanisms are checked separately). */
const ablateAll = reachable.map((s) => s.id).join(",");

const WAKE_KINDS: WakeKind[] = ["task", "monitor", "subagent-handover", "subagent-done", "supervisor-request"];
const allWakesSeen = (items: Item[]) => {
  const kinds = new Set(wakes(items).map((w) => w.wake.kind));
  return (
    WAKE_KINDS.every((k) => kinds.has(k)) &&
    wakes(items).some((w) => w.wake.status === "timeout") &&
    wakes(items).some((w) => w.wake.stillRunning.length > 0) &&
    // the agent_message "finished child" error (sent after subagent-done)
    toolResults(items).some((r) => r.toolName === "agent_message" && /does not resume children/.test(r.text))
  );
};

/**
 * Every string the faux model received in parent (or child) calls. By design
 * children load no extensions, so no ablation hook runs in child sessions.
 */
function stringsSeen(ep: FauxEpisode, who: "parent" | "child" = "parent"): string[] {
  const out: string[] = [];
  const walk = (v: unknown) => {
    if (typeof v === "string") out.push(v);
    else if (Array.isArray(v)) v.forEach(walk);
    else if (v && typeof v === "object") Object.values(v).forEach(walk);
  };
  const isParent = (c: FauxEpisode["calls"][number]) => {
    const u = c.messages.find((m) => m.role === "user");
    return JSON.stringify(u?.content ?? "") === JSON.stringify([{ type: "text", text: "go" }]);
  };
  for (const c of ep.calls.filter((c) => isParent(c) === (who === "parent"))) walk(c.messages);
  return out;
}

const bareSleepResult = (ep: FauxEpisode) => {
  const callIds = new Set(
    ep.items.flatMap((i) => (i.kind === "assistant" ? i.toolCalls.filter((c) => c.args.command === "sleep 1").map((c) => c.id) : [])),
  );
  return toolResults(ep.items).find((r) => callIds.has(r.toolCallId));
};

const episodes: FauxEpisode[] = [];
const tempDirs: string[] = [];
after(() => {
  episodes.forEach((e) => e.sandbox.cleanup());
  tempDirs.forEach((d) => rmSync(d, { recursive: true, force: true }));
});

const exercise = (env: Record<string, string>) =>
  runFaux({
    script: "exercise-surfaces.ts",
    famulusConfig: { foregroundBudgetMs: 300 },
    extensions: [ABLATION_EXT],
    env,
    until: allWakesSeen,
    untilTimeoutMs: 20_000,
    quietMs: 1500,
  });

describe("ablation harness", { concurrency: true }, () => {
  it("baseline: every reachable manifest segment reaches the model verbatim", async () => {
    const ep = await exercise({ PI_FAMULUS_ABLATE: "baseline" });
    episodes.push(ep);
    assert.ok(allWakesSeen(ep.items), `not every surface was exercised: ${wakes(ep.items).map((w) => w.wake.kind)}`);
    const seen = stringsSeen(ep);
    const missing = compileTextSegments(reachable).filter((s) => !seen.some((str) => new RegExp(s.re.source).test(str)));
    assert.deepEqual(
      missing.map((s) => s.id),
      [],
      "manifest drift: these segments no longer match the extension's text (update ablation/manifest.json)",
    );
    const childSeen = stringsSeen(ep, "child");
    const childMissing = compileTextSegments(childOnly).filter((s) => !childSeen.some((str) => new RegExp(s.re.source).test(str)));
    assert.deepEqual(childMissing.map((s) => s.id), [], "child-only segments drifted (checked in child calls)");
    // Children are isolated: the parent-only guidelines section must not leak into them.
    assert.ok(!childSeen.some((str) => str.includes("Background tasks and notifications (pi-famulus)")), "parent guidelines leaked into a child");
    const sleep = bareSleepResult(ep);
    assert.ok(sleep?.isError && /Refusing/.test(sleep.text), `bare sleep was not rejected in baseline: ${sleep?.text}`);
  });

  it("everything ablated: nothing left, every removal audited, wakes still delivered", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pi-famulus-eval-audit-"));
    tempDirs.push(dir);
    const logPath = join(dir, "ablation.jsonl");
    const ep = await exercise({ PI_FAMULUS_ABLATE: ablateAll, PI_FAMULUS_ABLATION_LOG: logPath });
    episodes.push(ep);
    assert.ok(allWakesSeen(ep.items), `wakes lost under ablation: ${wakes(ep.items).map((w) => w.wake.kind)}`);

    const seen = stringsSeen(ep);
    const leaked = compileTextSegments(textSegments).filter((s) => seen.some((str) => new RegExp(s.re.source).test(str)));
    assert.deepEqual(leaked.map((s) => s.id), [], "segments still visible to the model after ablation");
    // Data envelopes survive; only instructions go.
    for (const kind of WAKE_KINDS) {
      assert.ok(seen.some((s) => s.includes(`<pi-famulus-wake kind="${kind}"`)), `<pi-famulus-wake kind="${kind}"> removed by ablation`);
    }
    assert.ok(seen.some((s) => s.includes("<still-running>")), "<still-running> removed by ablation");

    const hits = new Map<string, number>();
    for (const line of readFileSync(logPath, "utf8").split("\n").filter(Boolean)) {
      const rec = JSON.parse(line) as { hook: string; hits: Record<string, number> };
      if (rec.hook === "init") continue;
      for (const [id, n] of Object.entries(rec.hits)) hits.set(id, (hits.get(id) ?? 0) + n);
    }
    const unaudited = reachable.map((s) => s.id).filter((id) => !((hits.get(id) ?? 0) > 0));
    assert.deepEqual(unaudited, [], "segments requested but never removed (audit log)");
  });

  it("supervisor-request wake (and its <reply-with>) reaches the model", async () => {
    const ep = await exercise({ PI_FAMULUS_ABLATE: "baseline" });
    episodes.push(ep);
    const req = wakes(ep.items).find((w) => w.wake.kind === "supervisor-request");
    assert.ok(req, "no supervisor-request wake: child could not call contact_supervisor");
    assert.match(req.wake.replyWith ?? "", /action: "reply"/);
    const seen = stringsSeen(ep);
    assert.ok(seen.some((str) => /<reply-with>/.test(str)), "<reply-with> not in the parent context");
  });

  it("mech.sleep-block: the bare sleep runs instead of being rejected", async () => {
    const ep = await exercise({ PI_FAMULUS_ABLATE: "mech.sleep-block" });
    episodes.push(ep);
    const sleep = bareSleepResult(ep);
    assert.ok(sleep && !sleep.isError, `bare sleep still rejected: ${sleep?.text}`);
  });
});
