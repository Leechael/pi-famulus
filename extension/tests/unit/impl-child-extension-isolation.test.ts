/**
 * Children are hosted by the daemon as their own `pi --mode rpc` processes
 * (createHostedSessionFn). Two children started together must not share
 * anything the parent hands them: each gets its own pi session, its own
 * system prompt file and its own environment, and extension module state
 * from the parent or a sibling never reaches it.
 *
 * The first describe checks what the parent passes to the daemon. The
 * second runs real children under the real daemon (PI_FAMULUS_INTEG=1).
 */
import { mkdtemp, readFile, rm, mkdir } from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { DefaultResourceLoader, SettingsManager } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../../src/manager-client";
import { createHostedSessionFn } from "../../src/subagent/hosted-session";
import type { ChildRunRequest } from "../../src/subagent/types";
import { FakeHostedManager, resetHostedFakes } from "./hosted-fakes";
import { FAUX_CHILD, HOSTED_INTEG, startHostedHarness, waitUntil, type HostedHarness } from "../integration/hosted-harness";

const PROBE = join(__dirname, "../fixtures/isolation-probe.js");

const tempDirs: string[] = [];

afterEach(async () => {
  resetHostedFakes();
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

function request(childId: string, systemPrompt: string): ChildRunRequest {
  return {
    childId,
    runId: "run_isolate",
    name: childId,
    prompt: "unused",
    timeoutMs: 10_000,
    depth: 1,
    agent: { name: "worker", description: "isolation probe", systemPrompt, source: "builtin" },
  };
}

function flag(argv: string[], name: string): string | undefined {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
}

describe("hosted child isolation: what the parent hands the daemon", () => {
  it("gives two concurrent children their own session, prompt file and environment", async () => {
    const home = await mkdtemp(join(tmpdir(), "pi-famulus-host-iso-"));
    tempDirs.push(home);
    const manager = new FakeHostedManager();
    const previous = process.env.PI_FAMULUS_PI;
    process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
    const sessions = [] as Array<{ dispose(): void | Promise<void> }>;
    try {
      const createSession = createHostedSessionFn({
        getModelRegistry: () => null,
        getParentModel: () => undefined,
        getParentThinkingLevel: () => "off",
        getCwd: () => home,
        getClient: () => manager as unknown as ManagerClient,
        getHome: () => home,
        extensionEntry: "/ext/index.ts",
      });
      const created = await Promise.all([
        createSession(request("ch_iso_a", "prompt for a")),
        createSession(request("ch_iso_b", "prompt for b")),
      ]);
      sessions.push(...created);
    } finally {
      if (previous === undefined) delete process.env.PI_FAMULUS_PI;
      else process.env.PI_FAMULUS_PI = previous;
    }
    try {
      const seen = await Promise.all(manager.spawns.map(async (spawn) => {
        const prompt = await readFile(flag(spawn.argv, "--append-system-prompt")!, "utf8");
        return {
          childId: spawn.childId,
          sessionId: flag(spawn.argv, "--session-id"),
          prompt,
          envName: spawn.env.PI_FAMULUS_CHILD_NAME,
        };
      }));
      expect(seen.sort((a, b) => a.childId.localeCompare(b.childId))).toEqual([
        { childId: "ch_iso_a", sessionId: "ch_iso_a", prompt: "prompt for a", envName: "ch_iso_a" },
        { childId: "ch_iso_b", sessionId: "ch_iso_b", prompt: "prompt for b", envName: "ch_iso_b" },
      ]);
      expect(manager.spawns[0].env).not.toBe(manager.spawns[1].env);
      expect(process.env.PI_FAMULUS_CHILD_NAME).toBeUndefined();
    } finally {
      await Promise.all(sessions.map((session) => session.dispose()));
    }
  });
});

interface ProbeRecord {
  id: string;
  pid: number;
  childId: string | null;
  seen: string;
  owners: string[];
}

describe.skipIf(!HOSTED_INTEG)("hosted child isolation: real children under the real daemon", () => {
  let harness: HostedHarness | null = null;

  afterEach(async () => {
    await harness?.cleanup();
    harness = null;
  });

  it("starts each child in its own process with none of the parent's or a sibling's module state", async () => {
    const root = await mkdtemp(join(tmpdir(), "pi-famulus-ext-iso-"));
    tempDirs.push(root);
    const logPath = join(root, "probe.jsonl");
    const cwd = join(root, "project");
    const agentDir = join(root, "agent");
    await mkdir(join(agentDir, "extensions"), { recursive: true });
    await mkdir(cwd, { recursive: true });
    process.env.PI_FAMULUS_TEST_PROBE_LOG = logPath;
    try {
      const parentLoader = new DefaultResourceLoader({
        cwd,
        agentDir,
        settingsManager: SettingsManager.create(cwd, agentDir, { projectTrusted: false }),
        additionalExtensionPaths: [PROBE],
      });
      await parentLoader.reload();
      expect(parentLoader.getExtensions().errors).toEqual([]);

      harness = await startHostedHarness({
        scenario: "hang",
        childArgs: ["-e", FAUX_CHILD, "-e", PROBE, "--model", "faux/faux-1"],
      });
      await harness.tool("subagent").execute(
        "sub-1",
        { async: true, tasks: [{ prompt: "one", name: "a" }, { prompt: "two", name: "b" }] },
        undefined,
        undefined,
        harness.ctx,
      );
      const records = () =>
        existsSync(logPath)
          ? readFileSync(logPath, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line) as ProbeRecord)
          : [];
      await waitUntil(() => records().filter((rec) => rec.childId).length >= 2, 60_000, "both children to load the probe");

      const [parent, ...children] = records();
      expect(parent).toMatchObject({ childId: null, pid: process.pid, seen: "init" });
      expect(children.map((child) => ({
        ownProcess: child.pid !== process.pid,
        seen: child.seen,
        owners: child.owners,
      }))).toEqual(children.map((child) => ({ ownProcess: true, seen: "init", owners: [child.id] })));
      expect(new Set(children.map((child) => child.pid)).size).toBe(2);
    } finally {
      delete process.env.PI_FAMULUS_TEST_PROBE_LOG;
    }
  }, 90_000);
});
