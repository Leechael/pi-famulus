/**
 * resolveModelSpec pure-function tests (design doc §4.6 model resolution, appendix B).
 */
import { describe, expect, it } from "vitest";
import {
  modelResolutionError,
  resolveModelSpec,
  splitThinkingSuffix,
  type ModelCandidate,
} from "../../src/subagent/model-spec";

const CANDIDATES: ModelCandidate[] = [
  { provider: "anthropic", id: "claude-opus-4-6", name: "Claude Opus 4.6" },
  { provider: "anthropic", id: "claude-haiku-4-5", name: "Claude Haiku 4.5" },
  { provider: "openai", id: "gpt-5.2", name: "GPT-5.2" },
  { provider: "openai", id: "gpt-5.2-mini", name: "GPT-5.2 Mini" },
  { provider: "openrouter", id: "openai/gpt-5.2:exacto" },
];

describe("splitThinkingSuffix", () => {
  it("strips a valid thinking level", () => {
    expect(splitThinkingSuffix("claude-haiku-4-5:high")).toEqual({
      base: "claude-haiku-4-5",
      thinking: "high",
    });
  });
  it("keeps unrecognized suffixes as part of the id", () => {
    expect(splitThinkingSuffix("openai/gpt-5.2:exacto")).toEqual({
      base: "openai/gpt-5.2:exacto",
      thinking: undefined,
    });
  });
  it("accepts every level pi core supports (parity with VALID_THINKING_LEVELS)", () => {
    for (const level of ["off", "minimal", "low", "medium", "high", "xhigh", "max"]) {
      expect(splitThinkingSuffix(`claude-haiku-4-5:${level}`)).toEqual({
        base: "claude-haiku-4-5",
        thinking: level,
      });
    }
  });
  it("ignores trailing/leading colons", () => {
    expect(splitThinkingSuffix(":")).toEqual({ base: ":" });
    expect(splitThinkingSuffix("model:")).toEqual({ base: "model:" });
  });
});

describe("resolveModelSpec", () => {
  it("resolves provider-qualified specs with / separator", () => {
    expect(resolveModelSpec("anthropic/claude-opus-4-6", CANDIDATES)).toEqual({
      ok: true,
      provider: "anthropic",
      id: "claude-opus-4-6",
      thinking: undefined,
    });
  });

  it("resolves provider-qualified specs with : separator", () => {
    expect(resolveModelSpec("openai:gpt-5.2", CANDIDATES)).toEqual({
      ok: true,
      provider: "openai",
      id: "gpt-5.2",
      thinking: undefined,
    });
  });

  it("resolves a bare id when unique across providers", () => {
    const res = resolveModelSpec("claude-haiku-4-5", CANDIDATES);
    expect(res).toMatchObject({ ok: true, provider: "anthropic" });
  });

  it("rejects an ambiguous bare id with the qualified matches", () => {
    const dupes: ModelCandidate[] = [
      { provider: "a", id: "same" },
      { provider: "b", id: "same" },
    ];
    const res = resolveModelSpec("same", dupes);
    expect(res).toMatchObject({ ok: false, error: "ambiguous" });
    expect((res as { candidates: string[] }).candidates).toEqual(["a/same", "b/same"]);
  });

  it("fuzzy-matches a unique substring of the id, case-insensitively", () => {
    const res = resolveModelSpec("HAIKU", CANDIDATES);
    expect(res).toMatchObject({ ok: true, id: "claude-haiku-4-5" });
  });

  it("fuzzy-matches display names", () => {
    const res = resolveModelSpec("opus 4.6", CANDIDATES);
    expect(res).toMatchObject({ ok: true, id: "claude-opus-4-6" });
  });

  it("reports ambiguity for multi-hit substrings", () => {
    const res = resolveModelSpec("gpt-5.2", CANDIDATES);
    // bare exact hit on openai/gpt-5.2 wins over substring ambiguity
    expect(res).toMatchObject({ ok: true, provider: "openai", id: "gpt-5.2" });
    const res2 = resolveModelSpec("mini", CANDIDATES);
    expect(res2).toMatchObject({ ok: true, id: "gpt-5.2-mini" });
  });

  it("handles ids containing separators (openrouter :exacto)", () => {
    const res = resolveModelSpec("openai/gpt-5.2:exacto", CANDIDATES);
    expect(res).toMatchObject({ ok: true, provider: "openrouter", id: "openai/gpt-5.2:exacto" });
  });

  it("parses thinking suffix together with a fuzzy base", () => {
    const res = resolveModelSpec("haiku:low", CANDIDATES);
    expect(res).toMatchObject({ ok: true, id: "claude-haiku-4-5", thinking: "low" });
  });

  it("no-match lists the full candidate set", () => {
    const res = resolveModelSpec("nonexistent", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect((res as { candidates: string[] }).candidates).toHaveLength(CANDIDATES.length);
  });

  it("empty candidate set yields no-match with empty list", () => {
    expect(resolveModelSpec("anything", [])).toMatchObject({
      ok: false,
      error: "no-match",
      candidates: [],
    });
  });
});

describe("thinking-suffix fallback (pi core parity)", () => {
  it("adapts an unknown suffix when the base resolves, with a warning", () => {
    const res = resolveModelSpec("claude-haiku-4-5:highest", CANDIDATES);
    expect(res).toMatchObject({ ok: true, provider: "anthropic", id: "claude-haiku-4-5" });
    expect(res).not.toHaveProperty("thinking");
    const warning = (res as { warning?: string }).warning ?? "";
    expect(warning).toContain('"highest"');
    expect(warning).toContain("off");
    expect(warning).toContain("max");
    expect(warning.toLowerCase()).toContain("default");
  });

  it("adapts an unknown suffix on provider-qualified specs", () => {
    const res = resolveModelSpec("openai/gpt-5.2:highest", CANDIDATES);
    expect(res).toMatchObject({ ok: true, provider: "openai", id: "gpt-5.2" });
    expect((res as { warning?: string }).warning).toContain('"highest"');
  });

  it("adapts an unknown suffix on a fuzzy base", () => {
    const res = resolveModelSpec("haiku:highest", CANDIDATES);
    expect(res).toMatchObject({ ok: true, id: "claude-haiku-4-5" });
  });

  it("prefers a literal id containing a colon over suffix stripping", () => {
    const res = resolveModelSpec("openai/gpt-5.2:exacto", CANDIDATES);
    expect(res).toMatchObject({
      ok: true,
      provider: "openrouter",
      id: "openai/gpt-5.2:exacto",
    });
    expect((res as { warning?: string }).warning).toBeUndefined();
  });

  it("no-match error names the invalid suffix and the retried base", () => {
    const res = resolveModelSpec("zzz:highest", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    const msg = modelResolutionError("zzz:highest", res as never);
    expect(msg).toContain('":highest"');
    expect(msg).toContain('"zzz"');
  });

  it("does not adapt past a full-spec ambiguity (ambiguity is semantic)", () => {
    const candidates: ModelCandidate[] = [
      { provider: "a", id: "m1", name: "x:foo one" },
      { provider: "b", id: "m2", name: "x:foo two" },
      { provider: "c", id: "x" },
    ];
    const res = resolveModelSpec("x:foo", candidates);
    expect(res).toMatchObject({ ok: false, error: "ambiguous" });
    expect((res as { candidates: string[] }).candidates.sort()).toEqual(["a/m1", "b/m2"]);
    expect(res).not.toHaveProperty("suffixHint");
  });

  it("ambiguous retried base names the dropped suffix in the error", () => {
    const dupes: ModelCandidate[] = [
      { provider: "a", id: "same" },
      { provider: "b", id: "same" },
    ];
    const res = resolveModelSpec("same:highest", dupes);
    expect(res).toMatchObject({ ok: false, error: "ambiguous" });
    const msg = modelResolutionError("same:highest", res as never);
    expect(msg).toContain("ambiguous on the retried base");
    expect(msg).toContain('":highest"');
    expect(msg).toContain('"same"');
    expect(msg).toContain("provider/<id>");
  });

  it("valid suffix with an unknown base fails without a suffix hint", () => {
    const res = resolveModelSpec("zzz:high", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("suffixHint");
  });

  it("trailing colon does not trigger the suffix retry", () => {
    const res = resolveModelSpec("claude-haiku-4-5:", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("suffixHint");
  });

  it("provider:id with an unknown id is a hard error, not a suffix retry", () => {
    // Without the guard, the retry reads ":nonexistent" as a thinking suffix,
    // retries the bare provider "openai", and fuzzy-matches the OpenRouter id
    // "openai/gpt-5.2:exacto" — silently swapping in an unrequested model.
    const res = resolveModelSpec("openai:nonexistent", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("suffixHint");
    expect(res).not.toHaveProperty("warning");
  });

  it("provider:<valid thinking> is a hard no-match, not a thinking split", () => {
    // off/max are newly recognized levels; without the provider-separator
    // check BEFORE splitThinkingSuffix they bypass the retry guard, and
    // matchBase("openai") unique-fuzzy-matches the OpenRouter id.
    for (const spec of ["openai:off", "openai:max"]) {
      const res = resolveModelSpec(spec, CANDIDATES);
      expect(res, spec).toMatchObject({ ok: false, error: "no-match" });
      expect(res, spec).not.toHaveProperty("thinking");
      expect(res, spec).not.toHaveProperty("warning");
      expect(res, spec).not.toHaveProperty("suffixHint");
    }
  });

  it("bare id with a valid thinking suffix still resolves (gpt-5.2:off)", () => {
    const res = resolveModelSpec("gpt-5.2:off", CANDIDATES);
    expect(res).toMatchObject({
      ok: true,
      provider: "openai",
      id: "gpt-5.2",
      thinking: "off",
    });
  });

  it("provider:id:thinking still resolves (openai:gpt-5.2:off)", () => {
    const res = resolveModelSpec("openai:gpt-5.2:off", CANDIDATES);
    expect(res).toMatchObject({
      ok: true,
      provider: "openai",
      id: "gpt-5.2",
      thinking: "off",
    });
  });

  it("provider:id with an unknown id fails even when the provider has one model", () => {
    const res = resolveModelSpec("openrouter:bogus", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("suffixHint");
  });

  it("provider-separator detection is case-insensitive", () => {
    // matchBase's qualified check is exact-case, so OpenAI:gpt-5.2 already
    // hard-fails; the guard must not then strip ":nonexistent" / ":off" and
    // fuzzy-match an unrelated id just because the provider casing differs.
    for (const spec of ["OpenAI:nonexistent", "OpenAI:off"]) {
      const res = resolveModelSpec(spec, CANDIDATES);
      expect(res, spec).toMatchObject({ ok: false, error: "no-match" });
      expect(res, spec).not.toHaveProperty("warning");
      expect(res, spec).not.toHaveProperty("thinking");
    }
  });

  it("openai:high does not fuzzy-match an OpenRouter model", () => {
    const res = resolveModelSpec("openai:high", CANDIDATES);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("provider", "openrouter");
  });

  it("openai:nonexistent is a hard no-match when openai is only an OpenRouter id prefix", () => {
    // Scoped whitelist omits the openai provider; the OpenRouter id still
    // contains "openai", so a retry of the bare prefix would unique-fuzzy-match
    // it. Nested id prefixes count as known providers so the separator holds.
    const scoped: ModelCandidate[] = [
      { provider: "openrouter", id: "openai/gpt-5.2:exacto" },
    ];
    const res = resolveModelSpec("openai:nonexistent", scoped);
    expect(res).toMatchObject({ ok: false, error: "no-match" });
    expect(res).not.toHaveProperty("warning");
    expect(res).not.toHaveProperty("suffixHint");
  });

  it("provider:id:<unknown suffix> still adapts (the last colon is not the separator)", () => {
    const res = resolveModelSpec("openai:gpt-5.2:highest", CANDIDATES);
    expect(res).toMatchObject({ ok: true, provider: "openai", id: "gpt-5.2" });
    expect((res as { warning?: string }).warning).toContain('"highest"');
  });

  it("ambiguous retried base reports the ambiguity, not the raw no-match", () => {
    const dupes: ModelCandidate[] = [
      { provider: "a", id: "same" },
      { provider: "b", id: "same" },
    ];
    const res = resolveModelSpec("same:highest", dupes);
    expect(res).toMatchObject({ ok: false, error: "ambiguous" });
    expect((res as { candidates: string[] }).candidates).toEqual(["a/same", "b/same"]);
  });
});

describe("modelResolutionError", () => {
  it("no-match message points at action:models", () => {
    const res = resolveModelSpec("zzz", CANDIDATES);
    const msg = modelResolutionError("zzz", res as never);
    expect(msg).toContain('"zzz"');
    expect(msg).toContain('action:"models"');
    expect(msg).toContain("anthropic/claude-opus-4-6");
  });
  it("ambiguous message suggests provider qualification", () => {
    const dupes: ModelCandidate[] = [
      { provider: "a", id: "x" },
      { provider: "b", id: "x" },
    ];
    const msg = modelResolutionError("x", resolveModelSpec("x", dupes) as never);
    expect(msg).toContain("ambiguous");
    expect(msg).toContain("provider/<id>");
  });
});
