export interface TokenUsage {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** Normalize provider usage counts while keeping missing values at zero. */
export function normalizedTokenUsage(usage: {
  input?: unknown;
  output?: unknown;
  cacheRead?: unknown;
  cacheWrite?: unknown;
}): TokenUsage {
  const count = (value: unknown): number => {
    const n = Number(value);
    return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
  };
  return {
    input: count(usage.input),
    output: count(usage.output),
    cacheRead: count(usage.cacheRead),
    cacheWrite: count(usage.cacheWrite),
  };
}

/** Add a provider message's usage to the child's cumulative totals. */
export function accumulateTokenUsage(total: TokenUsage, delta: TokenUsage): TokenUsage {
  return {
    input: total.input + delta.input,
    output: total.output + delta.output,
    cacheRead: total.cacheRead + delta.cacheRead,
    cacheWrite: total.cacheWrite + delta.cacheWrite,
  };
}

export function createTokenUsageAccumulator() {
  let total: TokenUsage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  return {
    add(usage: { input?: unknown; output?: unknown; cacheRead?: unknown; cacheWrite?: unknown }): TokenUsage {
      total = accumulateTokenUsage(total, normalizedTokenUsage(usage));
      return { ...total };
    },
    snapshot(): TokenUsage {
      return { ...total };
    },
  };
}
