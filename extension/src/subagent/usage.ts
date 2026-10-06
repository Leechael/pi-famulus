export interface TokenUsage {
  input: number;
  output: number;
}

/** Normalize provider usage counts while keeping missing values at zero. */
export function normalizedTokenUsage(usage: { input?: unknown; output?: unknown }): TokenUsage {
  const count = (value: unknown): number => {
    const n = Number(value);
    return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
  };
  return { input: count(usage.input), output: count(usage.output) };
}

/** Add a provider message's usage to the child's cumulative totals. */
export function accumulateTokenUsage(total: TokenUsage, delta: TokenUsage): TokenUsage {
  return { input: total.input + delta.input, output: total.output + delta.output };
}
