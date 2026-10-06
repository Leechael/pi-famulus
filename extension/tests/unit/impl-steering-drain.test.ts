import { describe, expect, it } from "vitest";
import { Agent } from "../../node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-agent-core/dist/agent.js";
import { AssistantMessageEventStream } from "../../node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist/utils/event-stream.js";

describe("pi steering queue drain", () => {
  it("injects multiple queued child steers in one request with all mode", async () => {
    let releaseFirst!: () => void;
    let markFirstCalled!: () => void;
    const firstCalled = new Promise<void>((resolve) => { markFirstCalled = resolve; });
    const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
    let requests = 0;
    let secondRequestMessages: string[] = [];
    const agent = new Agent({
      initialState: { model: {} as never },
      steeringMode: "all",
      streamFn: async (_model, context) => {
        requests++;
        if (requests === 1) {
          markFirstCalled();
          await firstGate;
        } else if (requests === 2) {
          secondRequestMessages = context.messages
            .filter((message) => message.role === "user")
            .flatMap((message) => typeof message.content === "string" ? [message.content] : []);
        }
        const stream = new AssistantMessageEventStream();
        stream.push({
          type: "done",
          message: {
            role: "assistant",
            content: [{ type: "text", text: "done" }],
            api: "openai-completions",
            provider: "test",
            model: "test",
            usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0 },
            stopReason: "stop",
            timestamp: Date.now(),
          },
        } as never);
        return stream;
      },
    });

    const run = agent.prompt("initial");
    await firstCalled;
    agent.steer({ role: "user", content: "steer one", timestamp: Date.now() });
    agent.steer({ role: "user", content: "steer two", timestamp: Date.now() });
    releaseFirst();
    await run;

    expect(requests).toBe(2);
    expect(secondRequestMessages).toEqual(["steer one", "steer two"]);
  });
});
