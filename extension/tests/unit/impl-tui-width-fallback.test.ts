import { afterEach, describe, expect, it, vi } from "vitest";
import { createMonitorTool, MonitorRegistry } from "../../src/monitor";
import { FAMULUS_WAKE_CUSTOM_TYPE } from "../../src/wake";
import { registerFamulusMessageRenderers } from "../../src/tui/message-renderers";
import { setPiTuiForTests, visibleWidth } from "../../src/tui/pi-tui-load";

const theme = {
  fg: (color: string, text: string) => `\x1b[31m${text}\x1b[0m`,
  bg: (_c: string, text: string) => text,
};

describe("tool rows stay within width without pi-tui", () => {
  afterEach(() => {
    setPiTuiForTests(undefined);
  });

  it("warns once when the reduced fallback is used", () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
    setPiTuiForTests(null);
    visibleWidth("first");
    visibleWidth("second");
    expect(warning).toHaveBeenCalledTimes(1);
    expect(warning.mock.calls[0]?.[0]).toContain("reduced text fallback");
    warning.mockRestore();
  });

  it("truncates a long monitor description when pi-tui cannot be resolved", () => {
    setPiTuiForTests(null);
    const tool = createMonitorTool(new MonitorRegistry({} as never));
    const desc = `watch ${"宽".repeat(40)} ${"x".repeat(80)}`;
    const component = tool.renderCall!({ command: "true", description: desc }, theme as never, {} as never);
    const lines = component.render(40);
    expect(lines.length).toBeGreaterThan(0);
    for (const line of lines) {
      expect(visibleWidth(line)).toBeLessThanOrEqual(40);
    }
    expect(lines.join("")).toContain("Monitor");
  });

  it("truncates a notification pill when pi-tui cannot be resolved", () => {
    setPiTuiForTests(null);
    const map = new Map<string, Function>();
    registerFamulusMessageRenderers({
      registerMessageRenderer(type: string, fn: unknown) {
        map.set(type, fn as never);
      },
    } as never);
    const summary = `Background command "${"宽".repeat(30)}${"x".repeat(80)}" failed`;
    const component = map.get(FAMULUS_WAKE_CUSTOM_TYPE)!(
      {
        content: `<pi-famulus-wake kind="task"><summary>${summary}</summary></pi-famulus-wake>`,
        details: {
          kind: "task",
          stillRunning: [],
          tasks: [
            {
              id: "sh_1",
              taskKind: "shell",
              status: "failed",
              summary,
              command: "x",
              outputPath: "/tmp/x",
              preview: "",
              durationMs: 1,
              exitCode: 1,
            },
          ],
        },
      },
      { expanded: false, outputPad: 1 },
      theme,
    );
    const lines = component.render(40);
    for (const line of lines) {
      expect(visibleWidth(line)).toBeLessThanOrEqual(40);
    }
    expect(lines.join("")).toContain("✗");
    expect(lines.join("")).not.toContain("✓");
  });
});
