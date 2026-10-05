/**
 * Thinking levels pi core accepts — the single source of truth for the
 * extension. The pi SDK does not export its list (pi's internal
 * VALID_THINKING_LEVELS), so this is a maintained copy; parity tests pin
 * every level (impl-model-spec.test.ts, impl-agents.test.ts).
 *
 * Pure module: zero pi dependencies, zero npm dependencies. Both src/agents/
 * and src/subagent/ consume this — neither layer imports the other (see
 * design doc Appendix B).
 */

export const VALID_THINKING_LEVELS = [
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const;

export type ThinkingLevel = (typeof VALID_THINKING_LEVELS)[number];

export function isValidThinkingLevel(level: string): level is ThinkingLevel {
  return (VALID_THINKING_LEVELS as readonly string[]).includes(level);
}
