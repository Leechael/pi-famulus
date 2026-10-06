import { describe, expect, it } from "vitest";
import { accumulateTokenUsage, normalizedTokenUsage } from "../../src/subagent/usage";

describe("subagent token usage", () => {
  it("accumulates provider-reported message_end usage without counting missing fields", () => {
    let total = { input: 0, output: 0 };
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: 120, output: 8 }));
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: 45, output: 13 }));
    total = accumulateTokenUsage(total, normalizedTokenUsage({ input: undefined, output: Number.NaN }));
    expect(total).toEqual({ input: 165, output: 21 });
  });
});
