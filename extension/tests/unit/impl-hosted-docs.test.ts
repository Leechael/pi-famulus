/**
 * Docs and the compat suite must describe the production hosted path,
 * not an in-process child session.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const root = join(import.meta.dirname, "..", "..", "..");

describe("hosted subagent path is what docs and CI describe", () => {
  it("does not tell readers that children run in-process via createAgentSession", () => {
    const tools = readFileSync(join(root, "docs", "tools.md"), "utf8");
    const design = readFileSync(join(root, "docs", "design.md"), "utf8");
    expect(tools.includes("in-process via `createAgentSession`")).toBe(false);
    expect(design.includes("in-process subagent children")).toBe(false);
    expect(tools.includes("daemon-hosted")).toBe(true);
  });
});
