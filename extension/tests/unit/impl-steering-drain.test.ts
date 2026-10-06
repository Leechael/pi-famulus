import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { createAgentSession, DefaultResourceLoader, SessionManager } from "@earendil-works/pi-coding-agent";
import { AssistantMessageEventStream } from "../../node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist/utils/event-stream.js";
import { childResourceLoaderOptions } from "../../src/subagent/pi-runtime";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("pi steering queue drain", () => {
  it("injects multiple queued child steers in one request after the session setter", async () => {
    const cwd = await mkdtemp(join(tmpdir(), "pi-famulus-steering-drain-"));
    tempDirs.push(cwd);
    const resourceLoader = new DefaultResourceLoader(childResourceLoaderOptions({ cwd, agentDir: cwd }));
    await resourceLoader.reload();
    const { session } = await createAgentSession({
      cwd,
      agentDir: cwd,
      noTools: "all",
      resourceLoader,
      sessionManager: SessionManager.inMemory(cwd),
    });

    let releaseFirst!: () => void;
    let markFirstCalled!: () => void;
    const firstCalled = new Promise<void>((resolve) => { markFirstCalled = resolve; });
    const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
    let requests = 0;
    let secondRequestMessages: string[] = [];
    session.agent.streamFunction = async (_model, context) => {
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
    };

    try {
      // This is the same setter call pi-runtime uses after createAgentSession.
      session.setSteeringMode("all");
      const run = session.agent.prompt("initial");
      await firstCalled;
      session.agent.steer({ role: "user", content: "steer one", timestamp: Date.now() });
      session.agent.steer({ role: "user", content: "steer two", timestamp: Date.now() });
      releaseFirst();
      await run;

      expect(requests).toBe(2);
      expect(secondRequestMessages).toEqual(["steer one", "steer two"]);
    } finally {
      session.dispose();
    }
  });
});
