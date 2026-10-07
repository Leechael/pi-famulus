import type { ContextWithSystemEvent } from "@earendil-works/pi-coding-agent";
import { describe, expect, it } from "vitest";
import { BEHAVIOR_GUIDELINES, restoreBehaviorGuidelines } from "../../src/behavior-guidelines";

type Messages = ContextWithSystemEvent["messages"];
const expected = `<pi-famulus>\n${BEHAVIOR_GUIDELINES}\n</pi-famulus>`;

describe("request-local guideline projection", () => {
  it("restores a removed section without mutating history or replacing unrelated sections", () => {
    const messages: Messages = [
      { role: "system", content: "Base instructions", sections: { "pi-famulus": expected, unrelated: "keep" }, timestamp: 1 },
      { role: "system", content: "", sections: { "pi-famulus": null, unrelated: "newer" }, timestamp: 2 },
    ];
    const original = structuredClone(messages);
    const result = restoreBehaviorGuidelines(messages)!;
    expect(messages).toEqual(original);
    expect(result.messages.slice(0, -1)).toEqual(messages);
    expect(result.messages[0]).toBe(messages[0]);
    expect(result.messages[1]).toBe(messages[1]);
    expect(result.messages.at(-1)).toMatchObject({ role: "system", content: "", sections: { "pi-famulus": expected } });
    expect(Object.keys((result.messages.at(-1) as { sections: object }).sections)).toEqual(["pi-famulus"]);
    expect(restoreBehaviorGuidelines(result.messages)).toBeUndefined();
  });

  it("leaves a current section alone and repairs stale sections", () => {
    const current: Messages = [{ role: "system", content: "", sections: { "pi-famulus": expected }, timestamp: 1 }];
    expect(restoreBehaviorGuidelines(current)).toBeUndefined();
    const stale: Messages = [{ role: "system", content: "", sections: { "pi-famulus": "old instructions" }, timestamp: 1 }];
    expect(restoreBehaviorGuidelines(stale)?.messages.at(-1)).toMatchObject({ sections: { "pi-famulus": expected } });
    expect(stale[0]).toMatchObject({ sections: { "pi-famulus": "old instructions" } });
  });

  it("adds a missing section while preserving a raw forced system prompt", () => {
    const messages: Messages = [{ role: "system", content: "Other extension's forced prompt", timestamp: 1 }];
    const result = restoreBehaviorGuidelines(messages)!;
    expect(result.messages[0]).toBe(messages[0]);
    expect(result.messages.at(-1)).toMatchObject({ sections: { "pi-famulus": expected } });
  });
});
