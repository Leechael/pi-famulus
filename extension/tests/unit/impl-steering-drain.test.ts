import { mkdtemp, rm } from "node:fs/promises";
import { findPackageJSON } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { createAgentSession, SessionManager } from "@earendil-works/pi-coding-agent";

// The SDK's own pi-ai, wherever the installer put it (nested by npm, hoisted
// or isolated by nub): the stream must be the class the session checks.
const sdkPackage = findPackageJSON("@earendil-works/pi-coding-agent", import.meta.url);
if (!sdkPackage) throw new Error("The pi SDK is missing");
const aiPackage = findPackageJSON("@earendil-works/pi-ai", pathToFileURL(join(dirname(sdkPackage), "dist/index.js")).href);
if (!aiPackage) throw new Error("The pi SDK's pi-ai dependency is missing");
const { AssistantMessageEventStream } = (await import(pathToFileURL(join(dirname(aiPackage), "dist/utils/event-stream.js")).href)) as {
  AssistantMessageEventStream: new () => { push(event: unknown): void };
};
import { createChildResources } from "../../src/subagent/pi-runtime";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("pi steering queue drain", () => {
  it("injects multiple queued child steers in one request after the session setter", async () => {
    const cwd = await mkdtemp(join(tmpdir(), "pi-famulus-steering-drain-"));
    tempDirs.push(cwd);
    const { resourceLoader, settingsManager } = await createChildResources({ cwd, agentDir: cwd, projectTrusted: false });
    const { session } = await createAgentSession({
      cwd,
      agentDir: cwd,
      noTools: "all",
      resourceLoader,
      settingsManager,
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
      // The SDK's class at runtime; its type is not reachable without a direct pi-ai dependency.
      return stream as never;
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
