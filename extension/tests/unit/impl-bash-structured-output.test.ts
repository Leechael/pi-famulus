import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import type { ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { createBashOverride } from "../../src/bash-override";

function ctx(): ExtensionToolContext {
  return {
    cwd: tmpdir(),
    sessionManager: { getSessionId: () => "s", getSessionFile: () => null },
  } as unknown as ExtensionToolContext;
}

function makeTool() {
  return createBashOverride({
    getClient: () => null, // force the local fallback path
    config: { foregroundBudgetMs: 20_000 } as never,
    home: join(tmpdir(), "pi-famulus-test"),
    sessionId: () => "s",
    sessionEnv: () => ({}),
    trackTask: vi.fn(),
    markNotifyOnExit: vi.fn(),
  });
}

describe("bash override structured output (pi ≥0.99 contract)", () => {
  it("resolves a non-zero exit to an isError result with structuredContent instead of rejecting", async () => {
    const tool = makeTool();
    const res = await tool.execute("tc1", { command: "echo partial-output; exit 3" }, undefined, undefined, ctx());
    expect(res.isError).toBe(true);
    const text = res.content.map((c) => (c.type === "text" ? c.text : "")).join("\n");
    expect(text).toContain("partial-output");
    expect(text).toContain("Command exited with code 3");
    expect(res.structuredContent).toMatchObject({
      exit_code: 3,
      truncated: false,
    });
    const structured = res.structuredContent as { output: string; wall_time_seconds: number };
    expect(structured.output).toContain("partial-output");
    expect(structured.wall_time_seconds).toBeGreaterThanOrEqual(0);
  });

  it("includes structuredContent with exit_code 0 on success", async () => {
    const tool = makeTool();
    const res = await tool.execute("tc2", { command: "echo ok-done" }, undefined, undefined, ctx());
    expect(res.isError).toBeUndefined();
    expect(res.structuredContent).toMatchObject({ exit_code: 0, truncated: false });
    expect((res.structuredContent as { output: string }).output).toContain("ok-done");
  });

  it("keeps the full 1 MiB tail when output is just over the structured cap", async () => {
    const tool = makeTool();
    // 1,048,577 bytes: exactly 1 byte over the 1 MiB structured cap.
    // head -c adds no trailing newline.
    const res = await tool.execute(
      "tc3",
      { command: "head -c 1048577 /dev/zero | tr '\\0' 'x'" },
      undefined,
      undefined,
      ctx(),
    );
    expect(res.isError).toBeUndefined();
    const structured = res.structuredContent as { output: string; truncated: boolean };
    expect(structured.truncated).toBe(true);
    expect(Buffer.byteLength(structured.output, "utf8")).toBe(1024 * 1024);
    expect(structured.output.endsWith("x")).toBe(true);
  });

  it("declares the outputSchema matching the built-in bash shape", () => {
    const tool = makeTool();
    const schema = tool.outputSchema as { properties: Record<string, unknown> };
    expect(Object.keys(schema.properties).sort()).toEqual([
      "exit_code",
      "full_output_path",
      "output",
      "truncated",
      "wall_time_seconds",
    ]);
  });
});
