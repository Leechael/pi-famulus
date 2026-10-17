import { describe, expect, it } from "vitest";
import { statusGlyph } from "../../src/tui/tool-component";

describe("TUI work status glyphs", () => {
  it("distinguishes active states from terminal success", () => {
    expect(statusGlyph("running")).toEqual({ color: "success", glyph: "●" });
    expect(statusGlyph("stopped")).toEqual({ color: "warning", glyph: "■" });
    expect(statusGlyph("pending")).toEqual({ color: "dim", glyph: "○" });
    expect(statusGlyph("completed")).toEqual({ color: "success", glyph: "✓" });
  });

  it("uses failure glyphs for failed and interrupted work", () => {
    expect(statusGlyph("failed")).toEqual({ color: "error", glyph: "✗" });
    expect(statusGlyph("interrupted")).toEqual({ color: "error", glyph: "✗" });
  });
});
