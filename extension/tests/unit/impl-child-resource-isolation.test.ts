import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { afterEach, describe, expect, it } from "vitest";
import { Type } from "typebox";
import { createAgentSession, DefaultResourceLoader, SessionManager } from "@earendil-works/pi-coding-agent";
import { BEHAVIOR_GUIDELINES, CHILD_BEHAVIOR_GUIDELINES } from "../../src/behavior-guidelines";
import { createChildBashTool } from "../../src/subagent/child-bash";
import {
  childResourceLoaderOptions,
  childSessionCreateOptions,
  createPiSessionFn,
} from "../../src/subagent/pi-runtime";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

async function fakeResources() {
  const root = await mkdtemp(join(tmpdir(), "pi-famulus-child-resources-"));
  tempDirs.push(root);
  const cwd = join(root, "project");
  const agentDir = join(root, "fake-home", ".pi", "agent");
  await mkdir(join(agentDir, "skills", "global-canary"), { recursive: true });
  await writeFile(
    join(agentDir, "skills", "global-canary", "SKILL.md"),
    "---\nname: global-canary\ndescription: fake global skill for child isolation\n---\nGLOBAL_CHILD_SKILL_CANARY",
  );
  await mkdir(join(agentDir, "extensions"), { recursive: true });
  await writeFile(
    join(agentDir, "extensions", "canary.ts"),
    `export default function (pi) {
  pi.registerTool({
    name: "canary_tool",
    label: "Canary",
    description: "Detect resource isolation regressions",
    promptGuidelines: ["EXT_CANARY"],
    parameters: { type: "object", properties: {} },
    async execute() { return { content: [{ type: "text", text: "canary" }] }; }
  });
  pi.on("before_agent_start", (event) => {
    event.systemPromptOptions.sections.ext_canary = "EXT_CANARY";
  });
}`,
  );
  await mkdir(cwd, { recursive: true });
  return { root, cwd, agentDir };
}

function injectedTools(root: string) {
  return [
    {
      name: "contact_supervisor",
      label: "Contact supervisor",
      description: "Contact supervisor",
      parameters: Type.Object({ reason: Type.String(), message: Type.String() }),
      async execute() {
        return { content: [{ type: "text" as const, text: "ok" }], details: undefined };
      },
    },
    {
      name: "agent_message",
      label: "Agent message",
      description: "Message a sibling",
      parameters: Type.Object({ action: Type.String() }),
      async execute() {
        return { content: [{ type: "text" as const, text: "ok" }], details: undefined };
      },
    },
    createChildBashTool({
      getClient: () => null,
      home: root,
      sessionId: () => "parent",
      sessionEnv: () => ({}),
      trackTask: () => {},
    }),
  ];
}

describe("child resource isolation", () => {
  it("filters skills and extensions from a fake global resource directory", async () => {
    const { cwd, agentDir } = await fakeResources();
    const defaultLoader = new DefaultResourceLoader({ cwd, agentDir });
    await defaultLoader.reload();
    expect(JSON.stringify(defaultLoader.getExtensions())).toContain("canary.ts");
    expect(defaultLoader.getSkills().skills.map((skill) => skill.name)).toContain("global-canary");

    const resourceLoader = new DefaultResourceLoader(childResourceLoaderOptions({ cwd, agentDir }));
    await resourceLoader.reload();
    const options = childSessionCreateOptions({
      cwd,
      model: undefined,
      thinkingLevel: "off",
      tools: ["read"],
      resourceLoader,
    });
    const { session, extensionsResult } = await createAgentSession({
      ...options,
      agentDir,
      sessionManager: SessionManager.inMemory(cwd),
    } as never);
    try {
      expect(extensionsResult.extensions).toHaveLength(0);
      expect(session.systemPrompt).not.toContain("GLOBAL_CHILD_SKILL_CANARY");
      expect(session.systemPrompt).not.toContain("global-canary");
      expect(resourceLoader.getSkills().skills).toHaveLength(0);
    } finally {
      session.dispose();
    }
  });

  it("uses the isolated loader in the production factory and exposes only the child's tools", async () => {
    const { root, cwd, agentDir } = await fakeResources();
    const oldAgentDir = process.env.PI_CODING_AGENT_DIR;
    process.env.PI_CODING_AGENT_DIR = agentDir;
    const createSession = createPiSessionFn({
      getModelRegistry: () => null,
      getParentModel: () => undefined,
      getParentThinkingLevel: () => "off",
      getCwd: () => cwd,
      customTools: () => injectedTools(root),
    });
    const request = (tools: string[]) => ({
      childId: "ch_canary01",
      runId: "run_canary01",
      name: "worker",
      prompt: "unused",
      timeoutMs: 10_000,
      depth: 1,
      agent: {
        name: "worker",
        description: "test child",
        tools,
        systemPrompt: "",
        source: "builtin" as const,
      },
    });

    try {
      const child = await createSession(request(["read", "bash"]));
      try {
        expect(child.getSystemPrompt?.()).toContain(CHILD_BEHAVIOR_GUIDELINES);
        expect(child.getSystemPrompt?.()).not.toContain(BEHAVIOR_GUIDELINES);
        expect(child.getSteeringMode?.()).toBe("all");
        expect(child.getSystemPrompt?.()).not.toContain("EXT_CANARY");
        expect(child.getSystemPrompt?.()).not.toContain("GLOBAL_CHILD_SKILL_CANARY");
        expect(child.getActiveToolNames?.()).toEqual(["read", "bash", "contact_supervisor", "agent_message"]);
      } finally {
        child.dispose();
      }

      // If the factory stops passing its isolated resourceLoader, this allowlisted
      // global extension tool reappears in the child's active tools.
      const canaryChild = await createSession(request(["read", "bash", "canary_tool"]));
      try {
        expect(canaryChild.getActiveToolNames?.()).toEqual(["read", "bash", "contact_supervisor", "agent_message"]);
        expect(canaryChild.getSystemPrompt?.()).not.toContain("EXT_CANARY");
      } finally {
        canaryChild.dispose();
      }
    } finally {
      if (oldAgentDir === undefined) delete process.env.PI_CODING_AGENT_DIR;
      else process.env.PI_CODING_AGENT_DIR = oldAgentDir;
    }
  });
});
