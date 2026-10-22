/**
 * I-55. An agent file that restricts tools (`tools: read`) must still leave
 * the child its two supervisor tools, contact_supervisor and agent_message
 * (design.md: they are always kept).
 *
 * The hosted child is `pi --mode rpc` started with the argv that
 * createHostedSessionFn builds. pi treats `--tools` as an allowlist for
 * every tool, including tools an extension registers (agent-session.js
 * _refreshToolRegistry filters registered tools with _isAllowedTool). So
 * this test takes the real argv, gives its `--tools` list to a real pi
 * session, and loads this extension's entry file into that session in child
 * mode, the way the child process does. No model and no daemon.
 */
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { createAgentSession, DefaultResourceLoader, SessionManager, SettingsManager } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../../src/manager-client";
import { createHostedSessionFn } from "../../src/subagent/hosted-session";
import { FakeHostedManager, resetHostedFakes } from "./hosted-fakes";

const EXTENSION_ENTRY = join(__dirname, "../../src/index.ts");
const tempDirs: string[] = [];
const savedEnv = new Map<string, string | undefined>();

function setEnv(key: string, value: string): void {
  if (!savedEnv.has(key)) savedEnv.set(key, process.env[key]);
  process.env[key] = value;
}

afterEach(async () => {
  for (const [key, value] of savedEnv) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  savedEnv.clear();
  resetHostedFakes();
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

/** The `--tools` list and environment of the hosted child's command line. */
async function hostedChildLaunch(home: string, tools: string[]): Promise<{ tools: string[] | undefined; env: Record<string, string> }> {
  const manager = new FakeHostedManager();
  setEnv("PI_FAMULUS_PI", JSON.stringify(["pi"]));
  const createSession = createHostedSessionFn({
    getModelRegistry: () => null,
    getParentModel: () => undefined,
    getParentThinkingLevel: () => "off",
    getCwd: () => home,
    getClient: () => manager as unknown as ManagerClient,
    getHome: () => home,
    extensionEntry: EXTENSION_ENTRY,
  });
  const session = await createSession({
    childId: "ch_allow",
    runId: "run_allow",
    name: "reader",
    prompt: "unused",
    timeoutMs: 10_000,
    depth: 1,
    agent: { name: "reader", description: "read-only agent", tools, systemPrompt: "", source: "user" },
  });
  session.dispose();
  const { argv, env } = manager.spawns[0];
  const i = argv.indexOf("--tools");
  return { tools: i >= 0 ? argv[i + 1].split(",") : undefined, env };
}

describe("I-55 a tool allowlist keeps the supervisor tools", () => {
  it("a hosted child of a `tools: read` agent can still call contact_supervisor and agent_message", async () => {
    const root = await mkdtemp(join(tmpdir(), "pi-famulus-allowlist-"));
    tempDirs.push(root);
    const cwd = join(root, "w");
    const agentDir = join(root, "a");
    const home = join(root, "h");
    for (const dir of [cwd, agentDir, home]) await mkdir(dir, { recursive: true });

    const { tools, env } = await hostedChildLaunch(home, ["read"]);

    // What the child process sees: child mode, and the parent's allowlist.
    setEnv("PI_FAMULUS_HOME", home);
    setEnv("PI_FAMULUS_CHILD_ID", "ch_allow");
    setEnv("PI_FAMULUS_CHILD_TOKEN", "test-token");
    if (env.PI_FAMULUS_CHILD_TOOLS !== undefined) setEnv("PI_FAMULUS_CHILD_TOOLS", env.PI_FAMULUS_CHILD_TOOLS);
    const settingsManager = SettingsManager.create(cwd, agentDir, { projectTrusted: false });
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir,
      settingsManager,
      additionalExtensionPaths: [EXTENSION_ENTRY],
    });
    await resourceLoader.reload();
    expect(resourceLoader.getExtensions().errors).toEqual([]);
    // Setup check: child mode registered both tools; only the allowlist can drop them.
    const registered = resourceLoader.getExtensions().extensions.flatMap((ext) => [...ext.tools.keys()]);
    expect(registered).toEqual(expect.arrayContaining(["contact_supervisor", "agent_message"]));
    const { session } = await createAgentSession({
      cwd,
      agentDir,
      settingsManager,
      resourceLoader,
      sessionManager: SessionManager.inMemory(cwd),
      ...(tools ? { tools } : {}),
    } as never);
    try {
      const active = session.getActiveToolNames();
      expect({
        read: active.includes("read"),
        write: active.includes("write"),
        contact_supervisor: active.includes("contact_supervisor"),
        agent_message: active.includes("agent_message"),
      }).toEqual({ read: true, write: false, contact_supervisor: true, agent_message: true });
    } finally {
      session.dispose();
    }
  }, 30_000);
});
