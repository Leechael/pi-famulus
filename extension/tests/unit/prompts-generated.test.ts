/**
 * src/prompts.generated.ts and prompts/INDEX.md are built from prompts/**.md.
 * A prompt edit without `npm run prompts` would ship the old text.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { parsePrompts, renderIndex, renderModule } from "../../scripts/build-prompts";

const root = join(import.meta.dirname, "..", "..");

describe("generated prompts", () => {
  const parsed = parsePrompts(join(root, "prompts"));
  it("src/prompts.generated.ts is up to date (run npm run prompts)", () => {
    expect(readFileSync(join(root, "src", "prompts.generated.ts"), "utf8").replace(/\r\n/g, "\n")).toBe(renderModule(parsed));
  });
  it("prompts/INDEX.md is up to date (run npm run prompts)", () => {
    expect(readFileSync(join(root, "prompts", "INDEX.md"), "utf8").replace(/\r\n/g, "\n")).toBe(renderIndex(parsed));
  });
  it("no prompt line ends in whitespace (editors strip it, silently changing the text)", () => {
    const trailing = parsed.prompts.filter((p) => (Array.isArray(p.value) ? p.value : [p.value]).some((v) => /[ \t]$/m.test(v)));
    expect(trailing.map((p) => p.id)).toEqual([]);
  });
});
