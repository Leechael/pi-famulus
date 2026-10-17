import { fitLines } from "./pi-tui-load";

function renderToolLines(lines: string[], width: number): string[] {
  const limit = Math.max(1, width);
  const out: string[] = [];
  for (const line of lines) out.push(...fitLines(line, limit));
  return out.length > 0 ? out : [""];
}

export function toolComponent(lines: string[]): { render(width: number): string[]; invalidate(): void } {
  return {
    render(width: number) {
      return renderToolLines(lines, width);
    },
    invalidate() {},
  };
}

/**
 * The one status mapping, shared with `pi-famulus top`: running ● green,
 * pending ○ dim, done ✓ green, failed ✗ red, stopped / timeout ■ amber.
 */
export function statusGlyph(status: string | undefined, isError = false): { color: string; glyph: string } {
  if (isError || status === "failed" || status === "killed" || status === "orphaned" || status === "interrupted") {
    return { color: "error", glyph: "✗" };
  }
  if (status === "running") return { color: "success", glyph: "●" };
  if (status === "pending") return { color: "dim", glyph: "○" };
  if (status === "partial" || status === "timeout" || status === "stopped") {
    return { color: "warning", glyph: "■" };
  }
  return { color: "success", glyph: "✓" };
}
