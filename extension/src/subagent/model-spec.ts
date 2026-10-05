/**
 * Fuzzy model spec resolution (design doc §4.6 "model resolution", appendix B).
 *
 * Pure and pi-free: candidates are plain {provider, id, name} records so the
 * resolver is testable without a ModelRegistry. The caller decides the
 * candidate set (scopedModels whitelist when non-empty, else all available).
 *
 * Spec grammar: `[provider/|provider:]id[:thinking]`
 *   - thinking suffix: one of VALID_THINKING_LEVELS (pi core parity;
 *     src/thinking-levels.ts is the single source of truth). Unrecognized
 *     suffixes are NOT silently kept in the id: when the full spec fails to
 *     match, the resolver retries without the trailing ":suffix" and adapts
 *     with a warning — the same semantics as pi's
 *     parseModelPattern(allowInvalidThinkingLevelFallback). The retry only
 *     fires on a full-spec no-match, so literal ids whose trailing ':segment'
 *     is not a valid level (OpenRouter ":exacto") still win at exact match.
 *     Note the converse: an id literally ending in a valid level (":high")
 *     is read as a thinking override, never as a literal id.
 *   - provider prefix: "provider/id" or "provider:id" both accepted
 *   - bare id: exact unique match, else case-insensitive substring on id/name
 */

export interface ModelCandidate {
  provider: string;
  id: string;
  name?: string;
}

export type ModelResolution =
  | { ok: true; provider: string; id: string; thinking?: string; warning?: string }
  | {
      ok: false;
      error: "no-match" | "ambiguous";
      candidates: string[];
      thinking?: string;
      /** Set when a trailing ":suffix" was stripped and retried unsuccessfully. */
      suffixHint?: string;
    };

/**
 * Thinking levels pi core accepts — re-exported from the single source of
 * truth (src/thinking-levels.ts) for convenience of resolver callers.
 */
export { VALID_THINKING_LEVELS } from "../thinking-levels";
import { VALID_THINKING_LEVELS } from "../thinking-levels";

const THINKING_LEVELS = new Set<string>(VALID_THINKING_LEVELS);

function label(c: ModelCandidate): string {
  return `${c.provider}/${c.id}`;
}

/** Strip a trailing ":<thinking>" when it is a valid level. */
export function splitThinkingSuffix(spec: string): { base: string; thinking?: string } {
  const idx = spec.lastIndexOf(":");
  if (idx <= 0 || idx === spec.length - 1) return { base: spec };
  const suffix = spec.slice(idx + 1).toLowerCase();
  if (!THINKING_LEVELS.has(suffix)) return { base: spec };
  return { base: spec.slice(0, idx), thinking: suffix };
}

function splitProviderPrefix(spec: string): { provider: string; id: string } | null {
  // "provider/id" or "provider:id" — first separator wins; the remainder may
  // itself contain separators (model ids with colons, e.g. ":exacto").
  for (const sep of ["/", ":"]) {
    const idx = spec.indexOf(sep);
    if (idx > 0 && idx < spec.length - 1) {
      return { provider: spec.slice(0, idx), id: spec.slice(idx + 1) };
    }
  }
  return null;
}

/** The three matching rules, shared by both resolution passes. */
function matchBase(base: string, candidates: ModelCandidate[]): ModelResolution {
  if (base.length === 0) {
    return { ok: false, error: "no-match", candidates: candidates.map(label) };
  }

  // 1) provider-qualified exact match
  const qualified = splitProviderPrefix(base);
  if (qualified) {
    const hit = candidates.find(
      (c) => c.provider === qualified.provider && c.id === qualified.id,
    );
    if (hit) return { ok: true, provider: hit.provider, id: hit.id };
    // Fall through: maybe the whole thing is a bare id containing a separator.
  }

  // 2) bare-id exact match (must be unique across providers)
  const exact = candidates.filter((c) => c.id === base);
  if (exact.length === 1) {
    return { ok: true, provider: exact[0].provider, id: exact[0].id };
  }
  if (exact.length > 1) {
    return { ok: false, error: "ambiguous", candidates: exact.map(label) };
  }

  // 3) case-insensitive substring on id or display name
  const needle = base.toLowerCase();
  const fuzzy = candidates.filter(
    (c) =>
      c.id.toLowerCase().includes(needle) ||
      (c.name !== undefined && c.name.toLowerCase().includes(needle)),
  );
  if (fuzzy.length === 1) {
    return { ok: true, provider: fuzzy[0].provider, id: fuzzy[0].id };
  }
  if (fuzzy.length > 1) {
    return { ok: false, error: "ambiguous", candidates: fuzzy.map(label) };
  }
  return { ok: false, error: "no-match", candidates: candidates.map(label) };
}

export function resolveModelSpec(spec: string, candidates: ModelCandidate[]): ModelResolution {
  const trimmed = spec.trim();
  const { base, thinking } = splitThinkingSuffix(trimmed);
  const first = matchBase(base, candidates);
  if (first.ok || thinking !== undefined || first.error === "ambiguous") {
    // Success, a valid thinking suffix, or an ambiguity: all final. Ambiguity
    // is semantic, not syntax — report it instead of adapting past it.
    return { ...first, thinking };
  }

  // Unknown trailing ":suffix" on a no-match: mirror pi core's
  // parseModelPattern(allowInvalidThinkingLevelFallback) — strip the suffix,
  // retry, and adapt with a warning instead of failing the whole call. The
  // retry only fires when the full spec matched nothing, so literal ids
  // containing a colon (OpenRouter ":exacto") are unaffected.
  const idx = trimmed.lastIndexOf(":");
  if (idx <= 0 || idx === trimmed.length - 1) return first;
  const suffix = trimmed.slice(idx + 1);
  const retriedBase = trimmed.slice(0, idx);
  const retried = matchBase(retriedBase, candidates);
  if (retried.ok) {
    return {
      ...retried,
      warning:
        `unknown thinking level "${suffix}" in model spec "${trimmed}"; ` +
        `using the model's default thinking ` +
        `(valid levels: ${VALID_THINKING_LEVELS.join(", ")})`,
    };
  }
  return {
    ...retried,
    suffixHint:
      `":${suffix}" is not a valid thinking level ` +
      `(valid: ${VALID_THINKING_LEVELS.join(", ")}); ` +
      `retried without it as "${retriedBase}"`,
  };
}

/** Render a resolution failure as an actionable error message. */
export function modelResolutionError(spec: string, res: ModelResolution & { ok: false }): string {
  const list = res.candidates.slice(0, 20).join(", ");
  const more = res.candidates.length > 20 ? `, … +${res.candidates.length - 20} more` : "";
  const hint = res.suffixHint ? ` (${res.suffixHint})` : "";
  if (res.error === "ambiguous") {
    // With a suffixHint the ambiguity belongs to the retried base, not the
    // full spec — say so, and name the dropped suffix either way.
    return (
      (res.suffixHint
        ? `model spec "${spec}" is ambiguous on the retried base`
        : `model spec "${spec}" is ambiguous`) +
      ` (matches: ${list}${more}).${hint} ` +
      `Qualify with "provider/<id>".`
    );
  }
  return (
    `model spec "${spec}" matched nothing.${hint} ` +
    `Available: ${list || "none"}${more}. ` +
    `Use subagent({action:"models"}) to list selectable models.`
  );
}
