import { appendFile, mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { afterEach, describe, expect, it } from "vitest";
import { Type } from "typebox";
import { createAgentSession, SessionManager } from "@earendil-works/pi-coding-agent";
import { BEHAVIOR_GUIDELINES, CHILD_BEHAVIOR_GUIDELINES } from "../../src/behavior-guidelines";
import { createChildBashTool } from "../../src/subagent/child-bash";
import { createChildResources, childSessionCreateOptions, createPiSessionFn, isFamulusChildSession } from "../../src/subagent/pi-runtime";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

async function fakeResources() {
  const root = await mkdtemp(join(tmpdir(), "pi-famulus-child-resources-"));
  tempDirs.push(root);
  const cwd = join(root, "project");
  const agentDir = join(root, "agent");
  await mkdir(join(agentDir, "skills", "global-canary"), { recursive: true });
  await writeFile(join(agentDir, "skills", "global-canary", "SKILL.md"),
    "---\nname: global-canary\ndescription: Global inheritance canary\n---\nGLOBAL_CHILD_SKILL_CANARY");
  await mkdir(join(agentDir, "extensions"), { recursive: true });
  await writeFile(join(agentDir, "extensions", "canary.ts"), `import { appendFile } from "node:fs/promises";
export default function (pi) {
  pi.on("session_shutdown", async () => {
    await appendFile(${JSON.stringify(join(root, "shutdown.txt"))}, "shutdown:first\\n");
  });
  pi.registerTool({
    name: "canary_tool", label: "Canary", description: "User extension",
    parameters: { type: "object", properties: {} },
    async execute() { return { content: [{ type: "text", text: "canary" }] }; }
  });
  pi.on("session_start", () => pi.registerTool({
    name: "startup_tool", label: "Startup", description: "User startup hook",
    parameters: { type: "object", properties: {} },
    async execute() { return { content: [{ type: "text", text: "started" }] }; }
  }));
  pi.registerTool({
    name: "mcp__canary__echo", label: "Deferred MCP", description: "MCP exposure canary",
    exposure: "codemode", parameters: { type: "object", properties: {} },
    async execute() { return { content: [{ type: "text", text: "mcp" }] }; }
  });
}`);
  await writeFile(join(agentDir, "extensions", "shutdown-canary.ts"), `import { appendFile } from "node:fs/promises";
export default function (pi) {
  pi.on("session_shutdown", async () => {
    await appendFile(${JSON.stringify(join(root, "shutdown.txt"))}, "shutdown:second\\n");
  });
}`);
  await writeFile(join(agentDir, "extensions", "famulus.ts"),
    `export { default } from ${JSON.stringify(resolve("src/index.ts"))};`);
  await mkdir(join(cwd, ".pi", "skills", "project-canary"), { recursive: true });
  await writeFile(join(cwd, ".pi", "skills", "project-canary", "SKILL.md"),
    "---\nname: project-canary\ndescription: Project inheritance canary\n---\nPROJECT_CHILD_SKILL_CANARY");
  await writeFile(join(cwd, "AGENTS.md"), "PROJECT_CONTEXT_CANARY");
  await writeFile(join(agentDir, "settings.json"), JSON.stringify({ defaultTools: ["+codemode"] }));
  return { root, cwd, agentDir };
}

function injectedTools(root: string) {
  return [
    ...["contact_supervisor", "agent_message"].map((name) => ({
      name, label: name, description: name, parameters: Type.Object({}),
      async execute() { return { content: [{ type: "text" as const, text: "ok" }], details: undefined }; },
    })),
    createChildBashTool({
      getClient: () => null, home: root, sessionId: () => "parent",
      sessionEnv: () => ({}), trackTask: () => {},
    }),
  ];
}

async function childSession(resources: Awaited<ReturnType<typeof fakeResources>>, tools?: string[], projectTrusted = true) {
  const { cwd, agentDir, root } = resources;
  const { resourceLoader, settingsManager } = await createChildResources({ cwd, agentDir, projectTrusted });
  const { session } = await createAgentSession({
    ...childSessionCreateOptions({
      cwd, model: undefined, thinkingLevel: "off", tools,
      customTools: injectedTools(root), resourceLoader,
    }),
    agentDir, settingsManager, sessionManager: SessionManager.inMemory(cwd),
  } as never);
  await session.bindExtensions({});
  return { session, resourceLoader, settingsManager };
}

describe("child resource inheritance", () => {
  it("fails closed when the extension event bus is missing", () => {
    expect(() => isFamulusChildSession({} as never)).toThrow("requires the extension event bus");
    expect(isFamulusChildSession({ events: { emit: (_channel: string, probe: { child: boolean }) => { probe.child = true; } } } as never)).toBe(true);
  });

  it("inherits defaults and user resources while skipping parent famulus initialization", async () => {
    const resources = await fakeResources();
    const settingsBefore = await readFile(join(resources.agentDir, "settings.json"), "utf8");
    const { session, resourceLoader, settingsManager } = await childSession(resources);
    try {
      expect(resourceLoader.getExtensions().errors).toEqual([]);
      expect(session.getActiveToolNames()).toEqual(expect.arrayContaining([
        "read", "bash", "edit", "write", "codemode", "canary_tool", "startup_tool",
        "contact_supervisor", "agent_message",
      ]));
      for (const name of ["subagent", "monitor", "task_list", "task_output", "task_stop"]) {
        expect(session.getToolDefinition(name)).toBeUndefined();
      }
      expect(resourceLoader.getSkills().skills.map((skill) => skill.name)).toEqual(
        expect.arrayContaining(["global-canary", "project-canary"]),
      );
      expect(session.systemPrompt).toContain("PROJECT_CONTEXT_CANARY");
      expect(session.systemPrompt).toContain(CHILD_BEHAVIOR_GUIDELINES);
      expect(session.systemPrompt).not.toContain(BEHAVIOR_GUIDELINES);
      expect(session.getToolDefinition("bash")?.parameters).not.toHaveProperty("properties.run_in_background");
      await resourceLoader.reload();
      expect(resourceLoader.getExtensions().errors).toEqual([]);
      expect(resourceLoader.getExtensions().extensions.flatMap((extension) => [...extension.tools.keys()])).not.toContain("subagent");
      expect(settingsManager.getDefaultTools()).toContain("codemode");
      expect(await readFile(join(resources.agentDir, "settings.json"), "utf8")).toBe(settingsBefore);
    } finally { session.dispose(); }
  });

  it("preserves explicit allowlists, including an empty list, while injecting communication tools", async () => {
    const resources = await fakeResources();
    for (const tools of [["read", "canary_tool"], []]) {
      const { session } = await childSession(resources, tools);
      try {
        expect(session.getActiveToolNames()).toEqual([...tools, "contact_supervisor", "agent_message"]);
        expect(session.getToolDefinition("codemode")).toBeUndefined();
        expect(session.getToolDefinition("mcp__canary__echo")).toBeUndefined();
      } finally { session.dispose(); }
    }
  });

  it("keeps explicitly allowlisted MCP tools callable through codemode", async () => {
    const resources = await fakeResources();
    const { session } = await childSession(resources, ["codemode", "mcp__canary__echo"]);
    try {
      expect(session.getToolDefinition("mcp__canary__echo")).toBeDefined();
      expect(session.getCallableToolNames()).toContain("mcp__canary__echo");
      expect(session.getToolDefinition("canary_tool")).toBeUndefined();
      expect(session.getActiveToolNames()).toContain("codemode");
    } finally { session.dispose(); }
  });

  it("honors user-disabled builtins rather than force-enabling codemode", async () => {
    const resources = await fakeResources();
    await writeFile(join(resources.agentDir, "settings.json"), JSON.stringify({
      defaultTools: ["+codemode"], extensions: ["-builtin:codemode"],
    }));
    const { session } = await childSession(resources);
    try { expect(session.getToolDefinition("codemode")).toBeUndefined(); }
    finally { session.dispose(); }
  });

  it("preserves project trust and configuration path provenance", async () => {
    const resources = await fakeResources();
    await mkdir(join(resources.cwd, ".pi", "project-prompts"), { recursive: true });
    await writeFile(join(resources.cwd, ".pi", "project-prompts", "canary.md"), "PROJECT_PROMPT_CANARY");
    await writeFile(join(resources.cwd, ".pi", "settings.json"), JSON.stringify({
      defaultTools: ["+grep"], prompts: ["./project-prompts"],
    }));
    for (const trusted of [true, false]) {
      const { session, resourceLoader, settingsManager } = await childSession(resources, undefined, trusted);
      try {
        expect(settingsManager.isProjectTrusted()).toBe(trusted);
        expect(session.getActiveToolNames().includes("grep")).toBe(trusted);
        expect(resourceLoader.getSkills().skills.some((skill) => skill.name === "project-canary")).toBe(trusted);
        expect(resourceLoader.getPrompts().prompts.some((prompt) => prompt.content.includes("PROJECT_PROMPT_CANARY"))).toBe(trusted);
      } finally { session.dispose(); }
    }
  });

  it("honors user-disabled bash and codemode-only settings", async () => {
    const resources = await fakeResources();
    await writeFile(join(resources.agentDir, "settings.json"), JSON.stringify({
      defaultTools: ["+codemode", "-bash"], codemode: { mode: "only" },
    }));
    const { session } = await childSession(resources);
    try {
      expect(session.getActiveToolNames()).toContain("codemode");
      expect(session.getActiveToolNames()).not.toContain("bash");
      expect(session.settingsManager.getSettings().codemode?.mode).toBe("only");
      expect(session.getCallableToolNames()).toEqual(expect.arrayContaining([
        "contact_supervisor", "agent_message", "mcp__canary__echo",
      ]));
      const codemode = session.agent.state.tools.find((tool) => tool.name === "codemode")!;
      const code = 'return await tools.contact_supervisor({});';
      session.sessionManager.appendMessage({
        role: "assistant", content: [{ type: "toolCall", id: "tc_inheritance", name: "codemode", arguments: { code } }],
        api: "openai-completions", provider: "test", model: "test", stopReason: "toolUse", timestamp: Date.now(),
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
          cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
      });
      session.agent.state.messages = session.sessionManager.buildSessionContext().messages;
      const result = await codemode.execute("tc_inheritance", { code });
      expect(result.content).toEqual(expect.arrayContaining([expect.objectContaining({ type: "text", text: expect.stringContaining("ok") })]));
    } finally { session.dispose(); }
  });

  it("uses inheritance and startup hooks in the production factory", async () => {
    const resources = await fakeResources();
    const createSession = createPiSessionFn({
      getModelRegistry: () => null, getParentModel: () => undefined,
      getParentThinkingLevel: () => "off", getCwd: () => resources.cwd,
      getAgentDir: () => resources.agentDir, getProjectTrusted: () => true,
      customTools: () => injectedTools(resources.root),
    });
    const request = {
      childId: "ch_canary01", runId: "run_canary01", name: "worker", prompt: "unused",
      timeoutMs: 10_000, depth: 1,
      agent: { name: "worker", description: "test child", systemPrompt: "", source: "builtin" as const },
    };
    const [first, second] = await Promise.all([createSession(request), createSession({ ...request, childId: "ch_canary02" })]);
    try {
      for (const child of [first, second]) {
        expect(child.getActiveToolNames?.()).toEqual(expect.arrayContaining(["codemode", "canary_tool", "startup_tool", "contact_supervisor"]));
        expect(child.getActiveToolNames?.()).not.toContain("subagent");
        expect(child.getSteeringMode?.()).toBe("all");
        expect(child.getSystemPrompt?.()).toContain("PROJECT_CONTEXT_CANARY");
      }
    } finally { await Promise.all([first.dispose(), second.dispose()]); }
    const shutdownMarkers = (await readFile(join(resources.root, "shutdown.txt"), "utf8")).trim().split("\n");
    expect(shutdownMarkers.filter((marker) => marker === "shutdown:first")).toHaveLength(2);
    expect(shutdownMarkers.filter((marker) => marker === "shutdown:second")).toHaveLength(2);
  });
});
