/** Narrow compatibility-only error precedence; no episode/runtime imports. */
import type { Grade } from "./graders.ts";

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
