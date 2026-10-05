import { describe, expect, it } from "vitest";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  DEFAULT_SUBAGENT_CONFIG,
  loadConfig,
  MAX_TIMER_DELAY_MS,
  resolveSubagentConfig,
} from "../../src/config";
import type { FamulusConfig } from "../../src/config";

function makeConfig(subagent?: FamulusConfig["subagent"]): FamulusConfig {
  return {
    foregroundBudgetMs: 20000,
    subagentBudgetMs: 45000,
    managerPath: null,
    logLevel: "info",
    ...(subagent !== undefined ? { subagent } : {}),
  };
}

describe("resolveSubagentConfig stall retries", () => {
  it("defaults to one auto-resume after a 5s delay", () => {
    const resolved = resolveSubagentConfig(makeConfig());
    expect(resolved.stallRetries).toBe(1);
    expect(resolved.stallRetryDelayMs).toBe(5000);
    expect(DEFAULT_SUBAGENT_CONFIG.stallRetries).toBe(1);
  });

  it("reads stallRetries and stallRetryDelayMs from the subagent section", () => {
    const resolved = resolveSubagentConfig(
      makeConfig({ stallRetries: 3, stallRetryDelayMs: 1000 }),
    );
    expect(resolved.stallRetries).toBe(3);
    expect(resolved.stallRetryDelayMs).toBe(1000);
  });

  it("allows 0 retries (settle stalled immediately, the pre-fix behavior)", () => {
    const resolved = resolveSubagentConfig(makeConfig({ stallRetries: 0 }));
    expect(resolved.stallRetries).toBe(0);
  });

  it("caps timer delays at the Node setTimeout maximum (P2)", () => {
    // Beyond ~2^31-1ms Node clamps setTimeout to ~1ms, which would fire the
    // retry almost immediately instead of waiting — cap instead.
    const resolved = resolveSubagentConfig(
      makeConfig({ stallMs: 1e308, stallRetryDelayMs: 1e308 }),
    );
    expect(resolved.stallMs).toBe(MAX_TIMER_DELAY_MS);
    expect(resolved.stallRetryDelayMs).toBe(MAX_TIMER_DELAY_MS);
  });

  it("floors fractional values and ignores negatives", () => {
    const resolved = resolveSubagentConfig(
      makeConfig({ stallRetries: 2.9, stallRetryDelayMs: -5 }),
    );
    expect(resolved.stallRetries).toBe(2);
    expect(resolved.stallRetryDelayMs).toBe(5000); // negative ignored
  });
});

describe("resolveSubagentConfig soft deadline", () => {
  it("defaults: 30 min soft budget, reminder every 10 min, no hard ceiling", () => {
    const resolved = resolveSubagentConfig(makeConfig());
    expect(resolved.timeoutMs).toBe(1_800_000);
    expect(resolved.overrunRepeatMs).toBe(600_000);
    expect(resolved.hardTimeoutMs).toBe(0);
  });

  it("reads overrunRepeatMs and hardTimeoutMs from the subagent section", () => {
    const resolved = resolveSubagentConfig(makeConfig({ overrunRepeatMs: 120_000, hardTimeoutMs: 7_200_000 }));
    expect(resolved.overrunRepeatMs).toBe(120_000);
    expect(resolved.hardTimeoutMs).toBe(7_200_000);
  });

  it("loads both keys from config.json", () => {
    const home = mkdtempSync(join(tmpdir(), "famulus-cfg-"));
    try {
      writeFileSync(join(home, "config.json"), JSON.stringify({ subagent: { overrunRepeatMs: 300_000, hardTimeoutMs: 5_400_000 } }));
      const resolved = resolveSubagentConfig(loadConfig(home));
      expect(resolved.overrunRepeatMs).toBe(300_000);
      expect(resolved.hardTimeoutMs).toBe(5_400_000);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  it("keeps the defaults for invalid values and caps timer delays", () => {
    const bad = resolveSubagentConfig(makeConfig({ overrunRepeatMs: 0, hardTimeoutMs: -1 }));
    expect(bad.overrunRepeatMs).toBe(600_000); // 0 would mean a reminder storm
    expect(bad.hardTimeoutMs).toBe(0);
    const huge = resolveSubagentConfig(makeConfig({ overrunRepeatMs: 1e308, hardTimeoutMs: 1e308 }));
    expect(huge.overrunRepeatMs).toBe(MAX_TIMER_DELAY_MS);
    expect(huge.hardTimeoutMs).toBe(MAX_TIMER_DELAY_MS);
  });
});
