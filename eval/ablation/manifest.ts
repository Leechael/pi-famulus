/**
 * Segment manifest: loading, variant expansion, and the text-removal engine.
 * Imported both by the runner (node) and by harness/ablation-ext.ts (inside
 * pi), so it must stay dependency-free.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { PROMPT_SEGMENTS } from "../../extension/src/prompts.generated.ts";

/**
 * A text/regex segment without its own text or pattern is the span marked
 * <!--seg:id--> in extension/prompts/ (one source of truth, no copy here).
 */
function resolveFromPrompts(s: Segment): void {
  if (s.text !== undefined || s.pattern !== undefined || (s.kind !== "text" && s.kind !== "regex")) return;
  const span = PROMPT_SEGMENTS[s.id];
  if (!span) throw new Error(`manifest segment ${s.id} has no text and no <!--seg:${s.id}--> span in extension/prompts/`);
  s.prompt = span.prompt;
  if (span.text !== undefined) {
    s.kind = "text";
    s.text = span.text;
  } else {
    s.kind = "regex";
    s.pattern = span.pattern;
  }
}

export interface Segment {
  id: string;
  kind: "text" | "regex" | "hook" | "config";
  surface: string;
  text?: string;
  /** Set on load: the extension prompt id that holds this span. */
  prompt?: string;
  pattern?: string;
  famulusConfig?: Record<string, unknown>;
  /** "child": only child sessions see it. */
  scope?: "parent" | "child";
  /** false: no external hook can remove it (see notAblatableExternally). Never scheduled as a variant. */
  ablatable?: boolean;
  /** Scenario ids expected to be affected; "*" = all (controls). */
  affects: string[];
  control?: boolean;
}

export const isAblatable = (s: Segment) => s.ablatable !== false;

export interface Group {
  id: string;
  segments: string[];
}

export interface Manifest {
  version: number;
  segments: Segment[];
  groups: Group[];
}

export const MANIFEST_PATH = join(import.meta.dirname, "manifest.json");

export function loadManifest(path = MANIFEST_PATH): Manifest {
  const m = JSON.parse(readFileSync(path, "utf8")) as Manifest;
  for (const s of m.segments) resolveFromPrompts(s);
  return m;
}

/** A variant = named set of removed segments. "baseline" removes nothing. */
export interface Variant {
  id: string;
  segments: Segment[];
}

export function resolveVariant(m: Manifest, id: string): Variant {
  if (id === "baseline") return { id, segments: [] };
  const seg = m.segments.find((s) => s.id === id);
  if (seg && seg.ablatable === false) throw new Error(`segment ${id} cannot be ablated externally (see notAblatableExternally)`);
  if (seg) return { id, segments: [seg] };
  const group = m.groups.find((g) => g.id === id);
  if (group) {
    return {
      id,
      segments: group.segments.map((sid) => {
        const s = m.segments.find((x) => x.id === sid);
        if (!s) throw new Error(`group ${id} references unknown segment ${sid}`);
        return s;
      }),
    };
  }
  throw new Error(`unknown variant "${id}" (baseline, a segment id, or a group id)`);
}

/** Comma-separated union of variants (used by the harness self-test). */
export function resolveVariantList(m: Manifest, ids: string): Variant {
  const parts = ids.split(",").map((s) => s.trim()).filter(Boolean);
  if (parts.length <= 1) return resolveVariant(m, parts[0] ?? "baseline");
  const segments = new Map<string, Segment>();
  for (const p of parts) for (const s of resolveVariant(m, p).segments) segments.set(s.id, s);
  return { id: ids, segments: [...segments.values()] };
}

/** Does this variant plausibly affect the scenario (per manifest `affects`)? */
export function variantAffects(v: Variant, scenarioId: string): boolean {
  if (v.id === "baseline") return true;
  return v.segments.some((s) => s.affects.includes("*") || s.affects.includes(scenarioId));
}

/** Merge config-kind mechanisms into the sandbox famulus config. */
export function variantFamulusConfig(v: Variant): Record<string, unknown> {
  return Object.assign({}, ...v.segments.filter((s) => s.kind === "config").map((s) => s.famulusConfig ?? {}));
}

// ---------------------------------------------------------------------------
// Removal engine
// ---------------------------------------------------------------------------

export interface CompiledSegment {
  id: string;
  re: RegExp;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function compileTextSegments(segments: Segment[]): CompiledSegment[] {
  return segments.flatMap((s) => {
    if (s.kind === "text" && s.text) return [{ id: s.id, re: new RegExp(escapeRe(s.text), "g") }];
    if (s.kind === "regex" && s.pattern) return [{ id: s.id, re: new RegExp(s.pattern, "g") }];
    return [];
  });
}

/** Remove all segments from one string; counts hits per segment id. */
export function removeSegments(input: string, segs: CompiledSegment[], hits: Map<string, number>): string {
  let out = input;
  let changed = false;
  for (const seg of segs) {
    seg.re.lastIndex = 0;
    const n = out.match(seg.re)?.length ?? 0;
    if (n === 0) continue;
    out = out.replace(seg.re, "");
    hits.set(seg.id, (hits.get(seg.id) ?? 0) + n);
    changed = true;
  }
  if (!changed) return input;
  // Drop bullets left empty and collapse the blank lines a removal leaves.
  return out.replace(/^[ \t]*- *$\n?/gm, "").replace(/\n{3,}/g, "\n\n");
}

/** Deep-walk a JSON-ish value, removing segments from every string. */
export function removeDeep<T>(value: T, segs: CompiledSegment[], hits: Map<string, number>): T {
  if (typeof value === "string") return removeSegments(value, segs, hits) as T;
  if (Array.isArray(value)) {
    let changed = false;
    const next = value.map((v) => {
      const r = removeDeep(v, segs, hits);
      if (r !== v) changed = true;
      return r;
    });
    return (changed ? next : value) as T;
  }
  if (value && typeof value === "object") {
    let changed = false;
    const next: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      const r = removeDeep(v, segs, hits);
      if (r !== v) changed = true;
      next[k] = r;
    }
    return (changed ? next : value) as T;
  }
  return value;
}

/** Same patterns as extension/src/bash-override.ts BARE_SLEEP_PATTERNS. */
export const BARE_SLEEP_PATTERNS: RegExp[] = [/^\s*sleep\s+\d/, /^\s*while\s+true\b/, /^\s*while\s+sleep\b/, /^\s*until\s+/];
