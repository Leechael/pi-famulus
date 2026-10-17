import { describe, expect, it } from "vitest";
import { parseToolCall, plainTheme, renderConversation, scrubTty, type ChatTheme } from "../../src/tui/conversation-view";
import { keyHints, paneLines, scrollThumb, spread } from "../../src/tui/scroll-detail-view";
import { turnsFromMessages } from "../../src/subagent/conversation";
import { visibleWidth } from "../../src/tui/pi-tui-load";

const tagged: ChatTheme = {
  fg: (color, text) => `<${color}>${text}</${color}>`,
  bold: (text) => `<b>${text}</b>`,
  italic: (text) => `<i>${text}</i>`,
};

const sample = turnsFromMessages([
  { role: "user", content: "inspect the loader please and then report back on what you find" },
  {
    role: "assistant",
    content: [
      { type: "thinking", thinking: "I should search first" },
      { type: "toolCall", name: "bash", arguments: { command: "rg loader" } },
    ],
  },
  { role: "toolResult", toolName: "bash", content: [{ type: "text", text: "src/loader.ts\nsrc/main.ts" }], isError: false },
  { role: "assistant", content: [{ type: "text", text: "Found it in `src/loader.ts`." }] },
]);

describe("conversation thread", () => {
  it("puts user turns on a rail and folds tools and thinking to one line", () => {
    const lines = renderConversation(sample, 40, plainTheme, { expanded: false });
    expect(lines.filter((l) => l.startsWith("┃ ")).length).toBeGreaterThanOrEqual(2);
    expect(lines).toContain("  ▸ ∴ Thinking");
    expect(lines.find((l) => l.includes("bash"))).toBe("  ▸ ✓ bash  rg loader");
    expect(lines.join("\n")).not.toContain("src/main.ts");
    expect(lines.join("\n")).not.toContain("I should search");
    expect(lines.join("\n")).not.toContain("── ");
    for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(40);
  });

  it("stacks activity rows tight and separates other blocks with one blank line", () => {
    const lines = renderConversation(sample, 80, plainTheme, { expanded: false, agent: "scout" });
    expect(lines).toEqual([
      "┃ main → scout",
      "┃ inspect the loader please and then report back on what you find",
      "",
      "● scout",
      "  ▸ ∴ Thinking",
      "  ▸ ✓ bash  rg loader",
      "",
      "  Found it in src/loader.ts.",
    ]);
  });

  it("unfolds output under a rule when expanded", () => {
    const lines = renderConversation(sample, 80, plainTheme, { expanded: true });
    expect(lines).toContain("  ▾ ✓ bash  rg loader");
    expect(lines).toContain("  │ src/loader.ts");
    expect(lines).toContain("  │ I should search first");
  });

  it("colors through the theme: accent rail, status glyphs, inline code", () => {
    const lines = renderConversation(sample, 80, tagged, { expanded: false });
    expect(lines[0].startsWith("<accent>┃ </accent>")).toBe(true);
    expect(lines.join("\n")).toContain("<success>✓ </success><b>bash</b>");
    expect(lines.join("\n")).toContain("<mdCode>src/loader.ts</mdCode>");
  });

  it("marks failed tools and pending calls", () => {
    const turns = [
      { role: "assistant", text: 'tool read {"path":"a.ts"}' },
      { role: "tool read error", text: "ENOENT" },
      { role: "assistant", text: 'tool bash {"command":"make"}' },
    ];
    const lines = renderConversation(turns, 60, plainTheme, { expanded: false });
    expect(lines[0]).toBe("● subagent");
    expect(lines[1]).toBe("  ▸ ✗ read  a.ts");
    expect(lines[2].startsWith("    ● bash  make")).toBe(true);
    expect(lines[2].endsWith("running")).toBe(true);
  });

  it("renders light markdown", () => {
    const text = "## Plan\n- one\n1. two\n> quoted\n```\ncode()\n```";
    const lines = renderConversation([{ role: "assistant", text }], 60, plainTheme, { expanded: false });
    expect(lines).toEqual(["● subagent", "  Plan", "  • one", "  1. two", "  │ quoted", "  │ code()"]);
  });

  it("shows model errors in red with a cross", () => {
    const lines = renderConversation([{ role: "assistant error", text: "529 overloaded" }], 60, tagged, { expanded: false });
    expect(lines).toEqual(["<success>● </success><b><success>subagent</success></b>", "  <error>✗ </error><error>529 overloaded</error>"]);
  });

  it("summarizes tool arguments without dumping file content", () => {
    expect(parseToolCall('tool write {"path":"/tmp/a.py","content":"import os\\nprint(1)"}')).toMatchObject({ name: "write", summary: "/tmp/a.py" });
    expect(parseToolCall('tool write {"path":"/tmp/cut.py","content":"unterminated')).toMatchObject({ name: "write", summary: "/tmp/cut.py" });
    expect(parseToolCall("hello there")).toBeUndefined();
  });

  it("scrubs terminal control sequences and carriage-return progress", () => {
    expect(scrubTty("\x1b[32mok\x1b[0m\r50%\r100%\nnext")).toBe("100%\nnext");
  });
});

describe("detail chrome helpers", () => {
  it("spreads left and right, dropping right when it does not fit", () => {
    expect(spread("ab", "cd", 20)).toBe(`ab${" ".repeat(16)}cd`);
    expect(spread("abcdef", "cd", 8)).toBe("abcdef");
  });

  it("renders key hints with bright keys and dim labels", () => {
    expect(keyHints(tagged, [["o", "unfold"]])).toBe("<b><muted>o</muted></b> <dim>unfold</dim>");
  });

  it("insets plain panes and hands renderers the inner width", () => {
    expect(paneLines(() => "hello", 20, plainTheme, false)).toEqual(["  hello"]);
    let seen = 0;
    paneLines({ render: (w) => { seen = w; return ["x"]; } }, 20, plainTheme, false);
    expect(seen).toBe(18);
  });

  it("moves the scroll thumb with progress and hides it when everything fits", () => {
    expect(scrollThumb(10, 20, 0)).toBeNull();
    const top = scrollThumb(100, 10, 0)!;
    const bottom = scrollThumb(100, 10, 90)!;
    expect(top.start).toBe(0);
    expect(bottom.end).toBe(10);
  });
});

describe("narrow terminals", () => {
  it("never paints a thread line wider than the pane", () => {
    for (let width = 1; width <= 60; width++) {
      for (const expanded of [false, true]) {
        for (const line of renderConversation(sample, width, plainTheme, { expanded, agent: "scout" })) {
          expect(visibleWidth(line), `width ${width}: ${JSON.stringify(line)}`).toBeLessThanOrEqual(width);
        }
      }
    }
  });
});

describe("write and edit calls", () => {
  it("show a line stat and unfold to numbered content, even when the transcript cut them", () => {
    const cut = `tool write {"path":"storage/final_verify.py","content":"from pathlib import Path\\nprint(1)\\nx = 1\\n… (3666 chars)`;
    const folded = renderConversation([{ role: "assistant", text: cut }], 80, plainTheme, { expanded: false });
    const row = folded.find((l) => l.includes("write"))!;
    expect(row).toContain("storage/final_verify.py");
    expect(row).toContain("+3+ lines");
    expect(folded.join("\n")).not.toContain("pathlib");
    const open = renderConversation([{ role: "assistant", text: cut }], 80, plainTheme, { expanded: true }).join("\n");
    expect(open).toContain("│ 1 from pathlib import Path");
    expect(open).toContain("cut short in the transcript");
  });

  it("show an edit as a -/+ stat and a diff per edit", () => {
    const call = 'tool edit {"path":"src/a.ts","edits":[{"oldText":"let a = 1;","newText":"let a = 2;\\nlet b = 3;"},{"oldText":"x","newText":"y"}]}';
    const lines = renderConversation(
      [{ role: "assistant", text: call }, { role: "tool edit", text: "Successfully replaced 2 blocks" }],
      90,
      plainTheme,
      { expanded: true },
    );
    const row = lines.find((l) => l.includes("edit") && l.includes("src/a.ts"))!;
    expect(row).toContain("−2 +3");
    expect(row).toContain("2 edits");
    expect(lines).toContain("  │ @@ edit 1/2");
    expect(lines).toContain("  │ - let a = 1;");
    expect(lines).toContain("  │ + let b = 3;");
    expect(lines.join("\n")).toContain("Successfully replaced");
  });
});

describe("review fixes", () => {
  it("pairs parallel calls with their own results, in order", () => {
    const turns = turnsFromMessages([
      { role: "assistant", content: [
        { type: "toolCall", name: "read", arguments: { path: "a.ts" } },
        { type: "toolCall", name: "bash", arguments: { command: "ls" } },
        { type: "toolCall", name: "read", arguments: { path: "b.ts" } },
      ] },
      { role: "toolResult", toolName: "read", content: [{ type: "text", text: "AAA" }] },
      { role: "toolResult", toolName: "bash", content: [{ type: "text", text: "LS" }] },
      { role: "toolResult", toolName: "read", content: [{ type: "text", text: "BBB" }] },
    ] as never);
    const out = renderConversation(turns, 80, plainTheme, { expanded: true }).map((l) => l.trim());
    expect(out.join("\n")).not.toContain("running");
    expect(out.indexOf("│ AAA")).toBe(out.findIndex((l) => l.includes("read  a.ts")) + 1);
    expect(out.indexOf("│ LS")).toBe(out.findIndex((l) => l.includes("bash  ls")) + 1);
    expect(out.indexOf("│ BBB")).toBe(out.findIndex((l) => l.includes("read  b.ts")) + 1);
  });

  it("keeps escape sequences in child prose away from the terminal", () => {
    const out = renderConversation(
      [{ role: "user", text: "hi \x1b]0;pwn\x07there" }, { role: "assistant", text: "ok \x1b[2Jdone" }],
      80, plainTheme, { expanded: false },
    ).join("\n");
    expect(out).not.toContain("\x1b");
    expect(out).toContain("ok done");
  });

  it("counts an empty write as zero lines", () => {
    const out = renderConversation([{ role: "assistant", text: 'tool write {"path":"e.txt","content":""}' }], 80, plainTheme, { expanded: false }).join("\n");
    expect(out).toContain("+0 lines");
    const blank = renderConversation([{ role: "assistant", text: 'tool write {"path":"e.txt","content":"\\n"}' }], 80, plainTheme, { expanded: false }).join("\n");
    expect(blank).toContain("+1 line");
  });

  it("notes a line the preview cap cut part-way through", () => {
    const content = [...Array.from({ length: 59 }, (_, i) => `l${i}`), "x".repeat(200)].join("\n");
    const text = `tool write ${JSON.stringify({ path: "w.txt", content })}`;
    const out = renderConversation([{ role: "assistant", text }], 40, plainTheme, { expanded: true }).join("\n");
    expect(out).toContain("… 1 more line");
  });
});
