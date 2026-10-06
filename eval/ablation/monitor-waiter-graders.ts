/** Compatibility-specific classification; wait_for is NOT globally polling. */
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { callsBetween, type CallAt } from "./graders.ts";
import { conditionError, matchesFixtureCondition } from "./fixtures/computer-use-0.5.1.ts";

export function waiterMisuses(items: Item[]): Array<{ call: CallAt; reasons: string[] }> {
  const results = toolResults(items);
  return callsBetween(items, -1).filter((c) => c.name === "wait_for").flatMap((call) => {
    const reasons: string[] = [];
    if (conditionError(call.args)) reasons.push("missing/invalid UI predicate");
    const knownState = typeof call.args.stateId === "string" && results.some((r) => {
      if (r.seq >= call.seq || r.isError || !["observe_ui", "wait_for"].includes(r.toolName)) return false;
      const capture = r.details?.capture as { stateId?: unknown } | undefined;
      return (capture?.stateId ?? r.details?.stateId) === call.args.stateId;
    });
    if (!knownState) reasons.push("fabricated/unobserved UI state");
    const predicate = [call.args.text, call.args.value, call.args.role, call.args.ref, call.args.scopeRef].filter(Boolean).join(" ");
    const monitorBodies = wakes(items).filter((w) => w.seq < call.seq && w.wake.kind === "monitor").map((w) => w.wake.body);
    if (/monitor|notification|checkpoint|service\.log|compat-source|COMPAT_EVENT|COMPAT_READY/i.test(predicate) ||
        monitorBodies.some((b) => /COMPAT_(?:EVENT|READY)/.test(b) && predicate && b.includes(predicate))) {
      reasons.push("monitor-source condition is not UI");
    } else if (!conditionError(call.args) && !matchesFixtureCondition(call.args)) {
      reasons.push("fabricated/non-fixture UI condition");
    }
    return reasons.length ? [{ call, reasons }] : [];
  });
}
