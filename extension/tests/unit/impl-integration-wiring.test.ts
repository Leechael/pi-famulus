/**
 * M3-M5 integration wiring tests (the seams the parallel agents could not
 * test): real SubagentRegistry <-> comms host adapter <-> comms tools, and
 * the subagent tool <-> real agents loader.
 */
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { resolveAgent } from "../../src/agents/definition";
import { createAgentLoader } from "../../src/agents/loader";
import { createComms } from "../../src/comms/comms";
import {
  createRegistryCommsHost,
} from "../../src/comms/registry-host";
import { createAgentMessageTool, createContactSupervisorTool } from "../../src/comms/tools";
import { SubagentRegistry } from "../../src/subagent/registry";
import { InProcessRunner } from "../../src/subagent/runner";
import { createSubagentTool } from "../../src/subagent/tool";
import type { ChildRunRequest } from "../../src/subagent/types";
import { SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

function makeStack() {
  const registry = new SubagentRegistry({ maxConcurrentChildren: 8, spawnBudgetPerHour: 32 });
  const factory = new SessionFactory();
  const runner = new InProcessRunner({
    createSession: factory.fn,
    acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
  });
  registry.setRunner(runner);
  const notifications: { customType: string; content: string; details?: unknown }[] = [];
  const host = createRegistryCommsHost({
    getRegistry: () => registry,
    getNotifyCenter: () => ({
      notify: (msg) => {
        notifications.push({ customType: msg.customType, content: msg.content, details: msg.details });
      },
    }),
  });
  const comms = createComms(host, { decisionTimeoutMs: 500 });
  return { registry, factory, host, comms, notifications };
}

function addReq(registry: SubagentRegistry, runId: string, name: string): ChildRunRequest {
  const childId = registry.addChild(runId, { name, agent: "worker" });
  return { childId, runId, name, prompt: `do ${name}`, agent: WORKER_AGENT, timeoutMs: 60_000, depth: 1 };
}

const execTool = (tool: { execute: (...a: never[]) => Promise<unknown> }, params: object) =>
  tool.execute("tc" as never, params as never, undefined as never, undefined as never, {} as never);

describe("registry-host adapter over a real registry", () => {
  it("maps children by id and name; sameRun reflects lineage", async () => {
    const { registry, factory, host } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    await registry.startChild(addReq(registry, run.runId, "alpha"));
    await registry.startChild(addReq(registry, run.runId, "beta"));
    const other = registry.createRun("tasks");
    await registry.startChild(addReq(registry, other.runId, "gamma"));

    const byName = host.getChild("alpha");
    expect(byName).toMatchObject({ runId: run.runId, name: "alpha", status: "running" });
    const alphaId = byName!.handle.childId;
    expect(host.getChild(alphaId)?.name).toBe("alpha");

    expect(host.listChildren()).toHaveLength(3);
    const betaId = host.getChild("beta")!.handle.childId;
    const gammaId = host.getChild("gamma")!.handle.childId;
    expect(host.sameRun(alphaId, betaId)).toBe(true);
    expect(host.sameRun(alphaId, gammaId)).toBe(false);
    expect(host.getChild("nonexistent")).toBeUndefined();
  });

  it("notifySupervisor reaches the NotifyCenter sink with the comms customType", () => {
    const { host, notifications } = makeStack();
    host.notifySupervisor({
      content: "<pi-famulus-wake kind=\"supervisor-update\">hi</pi-famulus-wake>",
      details: { kind: "supervisor-update", from: "ch_a", name: "alpha", message: "hi" },
    });
    expect(notifications).toEqual([
      {
        customType: "pi-famulus-wake",
        content: "<pi-famulus-wake kind=\"supervisor-update\">hi</pi-famulus-wake>",
        details: { kind: "supervisor-update", from: "ch_a", name: "alpha", message: "hi" },
      },
    ]);
  });
});

describe("comms tools over the real registry", () => {
  it("parent send steers a running child; send to a terminal child does not resume it", async () => {
    const { registry, factory, comms, host } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    await registry.startChild(addReq(registry, run.runId, "alpha"));
    const tool = createAgentMessageTool(comms, { kind: "parent" }, host);

    const r1 = (await execTool(tool, { action: "send", to: "alpha", message: "focus on tests" })) as {
      details: { ok: boolean };
    };
    expect(r1.details.ok).toBe(true);
    expect(factory.sessions[0].steers).toEqual(["focus on tests"]);

    // Finish, then send again → resume path (prompt on the same session).
    factory.sessions[0].complete("first done");
    await tick();
    const r2 = (await execTool(tool, { action: "send", to: "alpha", message: "now docs" })) as {
      details: { ok: boolean };
    };
    expect(r2.details.ok).toBe(false);
    expect(factory.sessions[0].prompts).toEqual(["do alpha"]);
  });

  it("need_decision blocks the child until the parent replies via the tool", async () => {
    const { registry, factory, comms, host, notifications } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "alpha");
    await registry.startChild(req);

    const childTool = createContactSupervisorTool(comms, req.childId);
    const pending = execTool(childTool, { reason: "need_decision", message: "drop the column?" });
    await tick();
    expect(notifications.some((n) => n.content.includes("drop the column?"))).toBe(true);
    expect(comms.pendingRequests().map((p) => p.childId)).toEqual([req.childId]);

    const parentTool = createAgentMessageTool(comms, { kind: "parent" }, host);
    await execTool(parentTool, { action: "reply", to: "alpha", message: "yes, drop it" });
    const childResult = (await pending) as { content: { text: string }[] };
    expect(childResult.content[0].text).toContain("yes, drop it");
  });

  it("need_decision times out with the contractual message", async () => {
    const { registry, factory, comms } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "alpha");
    await registry.startChild(req);
    const childTool = createContactSupervisorTool(comms, req.childId);
    const res = (await execTool(childTool, { reason: "need_decision", message: "anyone?" })) as {
      content: { text: string }[];
    };
    expect(res.content[0].text).toContain("did not respond");
  });

  it("dispose resolves orphaned waiters instead of hanging", async () => {
    const { registry, factory, comms } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    const req = addReq(registry, run.runId, "alpha");
    await registry.startChild(req);
    const childTool = createContactSupervisorTool(comms, req.childId);
    const pending = execTool(childTool, { reason: "need_decision", message: "stuck?" });
    comms.dispose();
    const res = (await pending) as { content: { text: string }[] };
    expect(res.content[0].text).toContain("did not respond");
  });

  it("child sender is confined to its own run (sibling ok, cross-run rejected)", async () => {
    const { registry, factory, comms, host } = makeStack();
    factory.autoComplete = null;
    const run = registry.createRun("tasks");
    await registry.startChild(addReq(registry, run.runId, "alpha"));
    await registry.startChild(addReq(registry, run.runId, "beta"));
    const other = registry.createRun("tasks");
    await registry.startChild(addReq(registry, other.runId, "gamma"));
    const alphaId = host.getChild("alpha")!.handle.childId;

    const childTool = createAgentMessageTool(
      comms,
      { kind: "child", childId: alphaId, runId: run.runId },
      host,
    );
    const okRes = (await execTool(childTool, { action: "send", to: "beta", message: "ping" })) as {
      details: { ok: boolean };
    };
    expect(okRes.details.ok).toBe(true);
    expect(factory.sessions[1].steers).toEqual(["ping"]);

    // F's tool contract: routing violations come back as error content, not throws.
    const denied = (await execTool(childTool, { action: "send", to: "gamma", message: "cross" })) as {
      content: { text: string }[];
      details: { ok: boolean };
    };
    expect(denied.details.ok).toBe(false);
    expect(denied.content[0].text).toMatch(/cross-run/);
    expect(factory.sessions[2].steers).toEqual([]);
  });
});

describe("subagent tool with the real agents loader", () => {
  let dir = "";
  afterEach(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
    dir = "";
  });

  it("resolves a project-level agent definition into the child request", async () => {
    dir = mkdtempSync(join(tmpdir(), "pi-famulus-agents-"));
    const projectAgents = join(dir, ".pi", "agents");
    mkdirSync(projectAgents, { recursive: true });
    writeFileSync(
      join(projectAgents, "scout.md"),
      "---\nname: scout\ndescription: test scout\ntools: [read]\n---\n\nYou scout ahead.\n",
    );
    const loader = createAgentLoader({
      userDir: join(dir, "nope-user"),
      projectDir: projectAgents,
    });

    const registry = new SubagentRegistry({ maxConcurrentChildren: 8, spawnBudgetPerHour: 32 });
    const factory = new SessionFactory(); // autoComplete "done"
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: (req, ticket) => registry.admitChild(req.childId, ticket),
    });
    registry.setRunner(runner);

    const tool = createSubagentTool({
      getRegistry: () => registry,
      getNotifyCenter: () => null,
      budgetMs: () => 5000,
      defaultTimeoutMs: 60_000,
      defaultConcurrency: 4,
      resolveAgent: (name) => resolveAgent(loader.reload().definitions, name),
    });

    const res = (await execTool(tool, { tasks: [{ agent: "scout", prompt: "look around" }] })) as {
      content: { text: string }[];
      details: { status: string };
    };
    expect(res.details.status).toBe("completed");
    expect(factory.requests[0].agent.name).toBe("scout");
    expect(factory.requests[0].agent.tools).toEqual(["read"]);
    expect(factory.requests[0].prompt).toContain("You scout ahead.");
    expect(factory.requests[0].prompt).toContain("look around");
  });

  it("unknown agent errors and lists available names (incl. builtins)", async () => {
    dir = mkdtempSync(join(tmpdir(), "pi-famulus-agents-"));
    const loader = createAgentLoader({
      userDir: join(dir, "nope-user"),
      projectDir: join(dir, "nope-project"),
    });
    const tool = createSubagentTool({
      getRegistry: () => {
        throw new Error("unreachable");
      },
      getNotifyCenter: () => null,
      budgetMs: () => 5000,
      defaultTimeoutMs: 60_000,
      defaultConcurrency: 4,
      resolveAgent: (name) => resolveAgent(loader.reload().definitions, name),
    });
    // resolveAgent throws before the registry is touched; assert via the loader directly.
    expect(() => resolveAgent(loader.reload().definitions, "nope")).toThrow(/worker/);
    expect(tool.name).toBe("subagent");
  });
});

describe('subagent action:"models"', () => {
  it("lists candidates with current/scoped marks", async () => {
    const tool = createSubagentTool({
      getRegistry: () => null,
      getNotifyCenter: () => null,
      budgetMs: () => 5000,
      defaultTimeoutMs: 60_000,
      defaultConcurrency: 4,
      listModels: () => [
        { provider: "anthropic", id: "claude-opus-4-6", name: "Claude Opus 4.6", current: true, scoped: true },
        { provider: "openai", id: "gpt-5.2", current: false, scoped: true },
      ],
    });
    const res = (await execTool(tool, { action: "models" })) as { content: { text: string }[] };
    const text = res.content[0].text;
    expect(text).toContain("whitelist");
    expect(text).toContain("anthropic/claude-opus-4-6 — Claude Opus 4.6 (current, scoped)");
    expect(text).toContain("openai/gpt-5.2 (scoped)");
  });

  it("errors clearly when the host cannot list models", async () => {
    const tool = createSubagentTool({
      getRegistry: () => null,
      getNotifyCenter: () => null,
      budgetMs: () => 5000,
      defaultTimeoutMs: 60_000,
      defaultConcurrency: 4,
    });
    await expect(execTool(tool, { action: "models" })).rejects.toThrow(/not available/);
  });
});
