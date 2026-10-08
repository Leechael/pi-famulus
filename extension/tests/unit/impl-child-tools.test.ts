import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it, vi } from "vitest";
import { Type } from "typebox";
import { createAgentSession, SessionManager } from "@earendil-works/pi-coding-agent";
import { createChildBashTool } from "../../src/subagent/child-bash";
import { childSessionCreateOptions, createChildResources, createPiSessionFn } from "../../src/subagent/pi-runtime";

async function localMcpFixture() {
  const root = await mkdtemp(join(tmpdir(), "pi-famulus-child-mcp-"));
  const cwd = join(root, "project");
  const agentDir = join(root, "agent");
  const marker = join(root, "exited");
  const listed = join(root, "listed");
  const server = join(root, "server.cjs");
  await mkdir(join(cwd, ".pi"), { recursive: true });
  await mkdir(agentDir, { recursive: true });
  await writeFile(server, `
const { writeFileSync } = require("node:fs");
const readline = require("node:readline");
process.on("exit", () => writeFileSync(${JSON.stringify(marker)}, String(process.pid)));
process.on("SIGTERM", () => process.exit(0));
process.stdin.on("end", () => process.exit(0));
readline.createInterface({ input: process.stdin }).on("line", line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  if (request.method === "initialize") {
    result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} },
      serverInfo: { name: "famulus-regression", version: "1.0.0" } };
  } else if (request.method === "tools/list") {
    writeFileSync(${JSON.stringify(listed)}, "listed");
    result = { tools: [{ name: "echo", description: "Local regression echo",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } }] };
  } else if (request.method === "tools/call") {
    result = { content: [{ type: "text", text: "LOCAL_MCP:" + request.params.arguments.text }] };
  } else if (request.method === "ping") result = {};
  else {
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: request.id,
      error: { code: -32601, message: "Unsupported method" } }) + "\\n");
    return;
  }
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }) + "\\n");
});
`);
  await writeFile(join(cwd, ".pi", "mcp.json"), JSON.stringify({
    mcpServers: { local: { command: process.execPath, args: [server], exposure: "codemode" } },
  }));
  return { root, cwd, agentDir, marker, listed };
}

async function expectMcpExited(marker: string) {
  await vi.waitFor(async () => {
    const pid = Number(await readFile(marker, "utf8"));
    expect(pid).toBeGreaterThan(0);
    expect(() => process.kill(pid, 0)).toThrow();
  }, { timeout: 5_000, interval: 20 });
}

const customTool = (name: string, parameters = Type.Object({})) => ({
  name,
  label: name,
  description: name,
  parameters,
  async execute() {
    return { content: [{ type: "text" as const, text: "ok" }] };
  },
});

describe("child custom tools", () => {
  it("registers inherited codemode MCP tools and calls the local stdio server without a model", async () => {
    const fixture = await localMcpFixture();
    let session: Awaited<ReturnType<typeof createAgentSession>>["session"] | undefined;
    try {
      const { resourceLoader, settingsManager } = await createChildResources({ ...fixture, projectTrusted: true });
      ({ session } = await createAgentSession({
        ...childSessionCreateOptions({ cwd: fixture.cwd, model: undefined, thinkingLevel: "off", resourceLoader }),
        agentDir: fixture.agentDir, settingsManager, sessionManager: SessionManager.inMemory(fixture.cwd),
      } as never));
      await session.bindExtensions({});
      await vi.waitFor(() => {
        expect(session!.getToolDefinition("mcp__local__echo")).toBeDefined();
      }, { timeout: 5_000, interval: 20 });
      expect(session.getActiveToolNames()).toContain("codemode");
      expect(session.getActiveToolNames()).not.toContain("mcp__local__echo");
      expect(session.getCallableToolNames()).toContain("mcp__local__echo");
      expect(session.getToolDefinition("mcp__local__echo")?.exposure).toBe("deferred");
      const definition = session.getToolDefinition("mcp__local__echo")!;
      const result = await definition.execute("local-call", { text: "child" }, undefined, undefined,
        session.extensionRunner.createContext() as never);
      expect(result.content).toContainEqual({ type: "text", text: "LOCAL_MCP:child" });
      await session.extensionRunner.emit({ type: "session_shutdown", reason: "quit" });
      await expectMcpExited(fixture.marker);
    } finally {
      if (session) {
        await session.extensionRunner.emit({ type: "session_shutdown", reason: "quit" });
        session.dispose();
      }
      await rm(fixture.root, { recursive: true, force: true });
    }
  }, 15_000);

  it("closes inherited MCP stdio processes when the production child adapter is disposed", async () => {
    const fixture = await localMcpFixture();
    let child: Awaited<ReturnType<ReturnType<typeof createPiSessionFn>>> | undefined;
    try {
      const createSession = createPiSessionFn({
        getModelRegistry: () => null, getParentModel: () => undefined,
        getParentThinkingLevel: () => "off", getCwd: () => fixture.cwd,
        getAgentDir: () => fixture.agentDir, getProjectTrusted: () => true,
      });
      child = await createSession({
        childId: "ch_mcp01", runId: "run_mcp01", name: "worker", prompt: "unused",
        timeoutMs: 10_000, depth: 1,
        agent: { name: "worker", description: "MCP regression", systemPrompt: "", source: "builtin" },
      });
      await vi.waitFor(async () => {
        expect(await readFile(fixture.listed, "utf8")).toBe("listed");
        expect(child!.getActiveToolNames?.()).toContain("codemode");
      }, { timeout: 5_000, interval: 20 });
      await child.dispose();
      await expectMcpExited(fixture.marker);
      // Disposal is idempotent even after extension contexts have been invalidated.
      await child.dispose();
    } finally {
      await child?.dispose();
      await rm(fixture.root, { recursive: true, force: true });
    }
  }, 15_000);

  it("keeps injected tools active under an agent allowlist and replaces bash without backgrounding", async () => {
    const customTools = [
      customTool("contact_supervisor"),
      customTool("agent_message"),
      createChildBashTool({
        getClient: () => null,
        home: "/tmp",
        sessionId: () => "parent",
        sessionEnv: () => ({}),
        trackTask: () => {},
      }),
    ];
    const options = childSessionCreateOptions({
      cwd: "/tmp",
      model: undefined,
      thinkingLevel: "off",
      tools: ["read", "bash"],
      customTools,
    });
    const { session } = await createAgentSession({
      ...options,
      agentDir: "/tmp/pi-famulus-child-tools-agent-dir",
      sessionManager: SessionManager.inMemory("/tmp"),
    } as never);
    try {
      expect(session.getActiveToolNames()).toEqual(["read", "bash", "contact_supervisor", "agent_message"]);
      expect(session.getToolDefinition("bash")?.parameters).not.toHaveProperty("properties.run_in_background");
    } finally {
      session.dispose();
    }
  });
});
