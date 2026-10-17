/**
 * In-process children load extensions through the same process-wide loader
 * cache as any earlier session. Each child must get its own extension
 * instance, and module state written for one child must not be visible to
 * another or inherited from the parent session.
 *
 * No model and no network: DefaultResourceLoader.reload instantiates the
 * extension, and createPiSessionFn is the production child path (it resolves
 * a missing model to the parent's, which is undefined here).
 */
import { mkdtemp, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { DefaultResourceLoader, SettingsManager } from "@earendil-works/pi-coding-agent";
import { createPiSessionFn } from "../../src/subagent/pi-runtime";
import type { ChildRunRequest } from "../../src/subagent/types";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

interface ProbeRecord {
  id: string;
  seen: string;
  reused: boolean;
  owners: string[];
}

function request(childId: string): ChildRunRequest {
  return {
    childId,
    runId: "run_isolate",
    name: childId,
    prompt: "unused",
    timeoutMs: 10_000,
    depth: 1,
    agent: { name: "worker", description: "isolation probe", systemPrompt: "", source: "builtin" },
  };
}

describe("in-process child extension isolation", () => {
  it.fails("gives each child its own instance and none of the parent's module state", async () => {
    const root = await mkdtemp(join(tmpdir(), "pi-famulus-ext-iso-"));
    tempDirs.push(root);
    const cwd = join(root, "project");
    const agentDir = join(root, "agent");
    const logPath = join(root, "probe.jsonl");
    await mkdir(join(agentDir, "extensions"), { recursive: true });
    await mkdir(cwd, { recursive: true });
    await writeFile(
      join(agentDir, "extensions", "probe.ts"),
      `import { appendFileSync } from "node:fs";
import { randomUUID } from "node:crypto";

const shared = { value: "init", owners: [] };
const apis = new Set();

export default function (pi) {
  const id = randomUUID();
  const reused = apis.has(pi);
  apis.add(pi);
  const seen = shared.value;
  shared.value = id;
  shared.owners.push(id);
  appendFileSync(${JSON.stringify(logPath)}, JSON.stringify({
    id,
    seen,
    reused,
    owners: shared.owners.slice(),
  }) + "\\n");
}
`,
    );

    const settingsManager = SettingsManager.create(cwd, agentDir, { projectTrusted: false });
    const parentLoader = new DefaultResourceLoader({ cwd, agentDir, settingsManager });
    await parentLoader.reload();
    expect(parentLoader.getExtensions().errors).toEqual([]);

    const createSession = createPiSessionFn({
      getModelRegistry: () => null,
      getParentModel: () => undefined,
      getParentThinkingLevel: () => "off",
      getCwd: () => cwd,
      getAgentDir: () => agentDir,
      getProjectTrusted: () => false,
    });
    const [first, second] = await Promise.all([
      createSession(request("ch_iso_a")),
      createSession(request("ch_iso_b")),
    ]);
    try {
      const lines = (await readFile(logPath, "utf8")).trim().split("\n").filter(Boolean);
      const records = lines.map((line) => JSON.parse(line) as ProbeRecord);
      expect(records).toHaveLength(3);
      const [parent, ...children] = records;
      expect(parent.seen).toBe("init");
      expect(parent.reused).toBe(false);
      expect(parent.owners).toEqual([parent.id]);
      expect(new Set(records.map((record) => record.id)).size).toBe(3);
      expect(children.map((child) => ({
        distinctFromParent: child.id !== parent.id,
        reused: child.reused,
        seen: child.seen,
        owners: child.owners,
      }))).toEqual(children.map((child) => ({
        distinctFromParent: true,
        reused: false,
        seen: "init",
        owners: [child.id],
      })));
    } finally {
      await Promise.all([first.dispose(), second.dispose()]);
    }
  }, 20_000);
});
