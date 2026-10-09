/** Episode error precedence; no episode/runtime imports. */
import type { RpcEvent } from "../lib/rpc.ts";
import { assistants, itemsFromEvents } from "../lib/transcript.ts";
import type { Grade } from "./graders.ts";

/** Classify the observed event prefix captured BEFORE harness shutdown. */
export function errorBeforeShutdown(events: RpcEvent[], grade: Grade, error?: string): string | undefined {
  let providerError: string | undefined;
  let unresolvedErrors = 0;
  let recoveredErrors = 0;
  for (const message of assistants(itemsFromEvents(events))) {
    if (message.stopReason === "error" || message.stopReason === "aborted") {
      providerError = `provider: ${message.errorMessage ?? message.stopReason}`;
      unresolvedErrors++;
    } else if (["stop", "toolUse", "length"].includes(message.stopReason ?? "")) {
      // A completed successful retry recovers earlier errors, not later ones.
      recoveredErrors += unresolvedErrors;
      unresolvedErrors = 0;
      providerError = undefined;
    }
  }
  if (recoveredErrors) grade.metrics.recoveredProviderErrors = recoveredErrors;

  // Transcript items omit message_start. An unfinished model response is an
  // infrastructure cutoff, unlike an assistant's completed, blocking tool call.
  let pendingResponse = false;
  for (const event of events) {
    const message = event.message as { role?: string; stopReason?: string } | undefined;
    if (event.type === "turn_start" || (event.type === "message_start" && message?.role === "assistant")) pendingResponse = true;
    if (event.type === "message_end" && message?.role === "assistant") pendingResponse = message.stopReason === "pending";
  }
  if (pendingResponse) grade.metrics.cutoffPendingResponse = true;
  return error ?? providerError ?? (pendingResponse && grade.pass !== true ? "episode cutoff: pending assistant response" : undefined);
}

/**
 * A proved bad waiter call remains a scored failure even if a later provider or
 * quiet-window error occurs. Preserve that error's provenance in metrics;
 * unrelated scenarios, INVALID grades, and unproved failures retain normal ERR.
 */
export function errorAfterCompatibilityGrade(
  scenario: { id: string; optIn?: boolean },
  grade: Grade,
  error: string | undefined,
): string | undefined {
  const badWaiters = grade.metrics.badWaiters;
  const provenSourceTamper = grade.metrics.fixtureSourceIntact === false;
  if (!scenario.optIn || !scenario.id.startsWith("monitor-waiter-") || grade.pass !== false ||
      !(typeof badWaiters === "number" && Number.isFinite(badWaiters) && badWaiters > 0) && !provenSourceTamper) return error;
  if (error !== undefined) grade.metrics.episodeError = error;
  return undefined;
}
