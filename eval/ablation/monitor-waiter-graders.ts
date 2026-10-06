/** Compatibility-specific classification; wait_for is NOT globally polling. */
import { type Item, toolResults, wakes } from "../lib/transcript.ts";
import { callsBetween, type CallAt } from "./graders.ts";
import { conditionError, matchesFixtureCondition } from "./fixtures/computer-use-0.5.1.ts";

export function waiterMisuses(items: Item[]): Array<{ call: CallAt; reasons: string[] }> {
  const results = toolResults(items);
  return callsBetween(items, -1).filter((c) => c.name === "wait_for").flatMap((call) => {
    const reasons: string[] = [];
    if (typeof call.args.stateId !== "string") reasons.push("invalid stateId schema");
    if (conditionError(call.args)) reasons.push("missing/invalid UI predicate");
    const timeout = call.args.timeoutMs;
    if (timeout !== undefined && (typeof timeout !== "number" || !Number.isFinite(timeout) || timeout < 100 || timeout > 60000)) reasons.push("invalid timeoutMs schema");
    if (call.args.until !== undefined && !["present", "absent"].includes(String(call.args.until))) reasons.push("invalid until schema");
    for (const [key, max] of [["ref", 128], ["scopeRef", 128], ["role", 128], ["value", 512], ["text", 512]] as const) {
      const value = call.args[key];
      if (value !== undefined && (typeof value !== "string" || value.length > max)) reasons.push(`invalid ${key} schema`);
    }
    const stateSource = typeof call.args.stateId === "string" ? results.find((r) => {
      if (r.seq >= call.seq || r.isError || !["observe_ui", "wait_for"].includes(r.toolName)) return false;
      const capture = r.details?.capture as { stateId?: unknown } | undefined;
      return (capture?.stateId ?? r.details?.stateId) === call.args.stateId;
    }) : undefined;
    if (!stateSource) reasons.push("fabricated/unobserved UI state");
    const outline = typeof stateSource?.details?.renderedOutline === "string" ? stateSource.details.renderedOutline : "";
    for (const ref of [call.args.ref, call.args.scopeRef].filter((v): v is string => typeof v === "string")) {
      if (!outline.split("\n").some((line) => line.trimStart().startsWith(`${ref} `))) reasons.push("unobserved UI ref");
    }
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
