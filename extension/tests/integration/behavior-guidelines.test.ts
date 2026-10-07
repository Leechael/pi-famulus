import { mkdtempSync, rmSync } from "node:fs";
import { createRequire, findPackageJSON } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import {
  createAgentSession,
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type ContextWithSystemEvent,
  type ProviderConfig,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { registerBehaviorGuidelines } from "../../src/behavior-guidelines";

// Use the SDK's bundled pi-ai so the fake provider and the session exercise
// the same version, without adding a second provider dependency to this package.
type RequestContext = Parameters<NonNullable<ProviderConfig["streamSimple"]>>[1];
type Assistant = Extract<RequestContext["messages"][number], { role: "assistant" }>;
type ToolCall = Extract<Assistant["content"][number], { type: "toolCall" }>;
type System = Extract<ContextWithSystemEvent["messages"][number], { role: "system" }>;
interface FauxExports {
  fauxProvider(): {
    provider: Parameters<ModelRuntime["registerNativeProvider"]>[0];
    getModel(): NonNullable<ReturnType<ModelRuntime["getModel"]>>;
    setResponses(responses: Array<(context: RequestContext) => Assistant>): void;
  };
  fauxAssistantMessage(content: string | ToolCall, options?: { stopReason: "toolUse" }): Assistant;
  fauxToolCall(name: string, args: Record<string, unknown>): ToolCall;
  getCurrentSystemMessage(messages: RequestContext["messages"]): System;
  getSystemMessageText(message: System): string;
}
const sdkPackage = findPackageJSON("@earendil-works/pi-coding-agent", import.meta.url);
if (!sdkPackage) throw new Error("The pi SDK is missing");
const sdkUrl = pathToFileURL(join(dirname(sdkPackage), "dist/index.js")).href;
const aiPackage = findPackageJSON("@earendil-works/pi-ai", sdkUrl);
if (!aiPackage) throw new Error("The pi SDK's pi-ai dependency is missing");
const { fauxProvider, fauxAssistantMessage, fauxToolCall, getCurrentSystemMessage, getSystemMessageText } =
  createRequire(sdkUrl)(join(dirname(aiPackage), "dist/index.js")) as FauxExports;

// Regression: session_start has no getSystemPromptOptions. An idle wake does
// not call before_agent_start; after its first tool result, pi rebuilds from
// the base prompt and removes the run-local pi-famulus section (2026-10-05).
describe("parent guidelines across the real pi session lifecycle", () => {
  it.each([true, false])("keeps guidelines across tool continuation and successive wakes (initial user turn: %s)", async (initialUserTurn) => {
    const dir = mkdtempSync(join(tmpdir(), "famulus-guidelines-"));
    const settingsManager = SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: false } });
    const faux = fauxProvider();
    const modelRuntime = await ModelRuntime.create({
      authPath: join(dir, "auth.json"),
      modelsPath: null,
      modelsStorePath: join(dir, "models-cache.json"),
      refreshOnCreate: false,
      allowModelNetwork: false,
    });
    modelRuntime.registerNativeProvider(faux.provider);
    await modelRuntime.setRuntimeApiKey(faux.provider.id, "offline-faux-key");
    const loader = new DefaultResourceLoader({
      cwd: dir,
      agentDir: dir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noThemes: true,
      noPromptTemplates: true,
      noContextFiles: true,
      extensionFactories: [(pi) => {
        registerBehaviorGuidelines(pi);
        pi.on("before_agent_start", (event) => {
          event.systemPromptOptions.sections!.unrelated = "Keep the other extension's instructions.";
        });
        pi.registerTool({
          name: "rearm_probe",
          label: "Offline re-arm",
          description: "Offline tool returning a monitor re-arm acknowledgement.",
          parameters: Type.Object({}),
          execute: async () => ({ content: [{ type: "text", text: "Monitor re-armed." }], details: {} }),
        });
      }],
    });
    await loader.reload();
    const { session } = await createAgentSession({
      cwd: dir,
      agentDir: dir,
      model: faux.getModel(),
      modelRuntime,
      resourceLoader: loader,
      sessionManager: SessionManager.inMemory(dir),
      settingsManager,
      noTools: "builtin",
    });
    try {
      await session.bindExtensions({ mode: "rpc" });
      const prompts: string[] = [];
      const response = (content: string | ToolCall) => (context: RequestContext): Assistant => {
        prompts.push(getSystemMessageText(getCurrentSystemMessage(context.messages)));
        return fauxAssistantMessage(content, typeof content === "string" ? undefined : { stopReason: "toolUse" });
      };
      faux.setResponses([
        ...(initialUserTurn ? [response("watching")] : []),
        response(fauxToolCall("rearm_probe", {})),
        response("re-armed; waiting for the next notification"),
        response("baseline unchanged"),
      ]);
      if (initialUserTurn) await session.prompt("Watch the service and re-arm on timeout.");
      await session.sendCustomMessage({
        customType: "pi-famulus-wake",
        content: '<pi-famulus-wake kind="monitor" status="timeout"><event>Re-arm if needed.</event></pi-famulus-wake>',
        display: true,
      }, { triggerTurn: true });
      await session.sendCustomMessage({
        customType: "pi-famulus-wake",
        content: '<pi-famulus-wake kind="monitor"><event>Baseline unchanged.</event></pi-famulus-wake>',
        display: true,
      }, { triggerTurn: true });
      expect(prompts).toHaveLength(initialUserTurn ? 4 : 3);
      for (const [index, prompt] of prompts.entries()) {
        expect(prompt, `request ${index}`).toContain("Background tasks and notifications (pi-famulus)");
      }
      if (initialUserTurn) expect(prompts[0]).toContain("Keep the other extension's instructions.");
      // Request-local repair must not change persisted context or tool loadout.
      expect(session.getActiveToolNames()).toEqual(["rearm_probe"]);
      expect(session.isIdle).toBe(true);
    } finally {
      session.dispose();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
