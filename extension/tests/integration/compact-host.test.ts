/**
 * Host compaction facts for the pinned SDK (1.0.0) and an optional global 1.4.0.
 * Model-visible = present in a streamFunction/request context, not message_end.
 */
import { mkdtemp, rm } from "node:fs/promises";
import { createRequire, findPackageJSON } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";
import { afterAll, describe, expect, it } from "vitest";

type Sdk = typeof import("@earendil-works/pi-coding-agent");

function loadSdk(root: string): { version: string; sdk: Sdk; ai: FauxAi } {
  const pkg = createRequire(import.meta.url)(join(root, "package.json")) as { version: string };
  const sdkUrl = pathToFileURL(join(root, "dist/index.js")).href;
  const aiPackage = findPackageJSON("@earendil-works/pi-ai", sdkUrl);
  if (!aiPackage) throw new Error(`pi-ai missing under ${root}`);
  const ai = createRequire(sdkUrl)(join(dirname(aiPackage), "dist/index.js")) as FauxAi;
  return { version: pkg.version, sdk: createRequire(sdkUrl)(join(root, "dist/index.js")) as Sdk, ai };
}

interface FauxAi {
  fauxProvider(options?: {
    models?: Array<{
      id: string;
      name: string;
      contextWindow: number;
      maxTokens: number;
      reasoning: boolean;
      input: string[];
      cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
    }>;
  }): {
    provider: unknown;
    getModel(): unknown;
    setResponses(responses: Array<(context: { messages: Array<{ role?: string; content?: unknown; customType?: string }> }) => unknown>): void;
  };
  fauxAssistantMessage(content: string): unknown;
}

function depRoot(): string {
  const pkg = findPackageJSON("@earendil-works/pi-coding-agent", import.meta.url);
  if (!pkg) throw new Error("dependency pi-coding-agent missing");
  return dirname(pkg);
}

function globalRoot(): string | undefined {
  try {
    const npmRoot = execFileSync("npm", ["root", "-g"], { encoding: "utf8" }).trim();
    const root = join(npmRoot, "@earendil-works/pi-coding-agent");
    createRequire(import.meta.url)(join(root, "package.json"));
    return root;
  } catch {
    return undefined;
  }
}

const hosts: { label: string; root: string }[] = [{ label: "dependency", root: depRoot() }];
const globalSdk = globalRoot();
if (globalSdk && globalSdk !== depRoot()) hosts.push({ label: "global", root: globalSdk });

const dirs: string[] = [];
afterAll(async () => {
  await Promise.all(dirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

function terminalNotice(tag: string) {
  return {
    customType: "pi-famulus-wake",
    content: `terminal:${tag}`,
    display: true,
    details: { kind: "monitor", id: "mon_1", status: "exited", event: tag },
  };
}

async function makeSession(host: { root: string }, modelOpts?: { contextWindow?: number }) {
  const { version, sdk, ai } = loadSdk(host.root);
  const cwd = await mkdtemp(join(tmpdir(), "fam-compact-host-"));
  dirs.push(cwd);
  const faux = ai.fauxProvider({
    models: [{
      id: "faux-1",
      name: "Faux",
      contextWindow: modelOpts?.contextWindow ?? 128000,
      maxTokens: 256,
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    }],
  });
  const modelRuntime = await sdk.ModelRuntime.create({
    authPath: join(cwd, "auth.json"),
    modelsPath: null,
    modelsStorePath: join(cwd, "models-cache.json"),
    refreshOnCreate: false,
    allowModelNetwork: false,
  });
  modelRuntime.registerNativeProvider(faux.provider as never);
  await modelRuntime.setRuntimeApiKey((faux.provider as { id: string }).id, "offline-faux-key");
  const settingsManager = sdk.SettingsManager.inMemory({
    compaction: { enabled: true, reserveTokens: 1, keepRecentTokens: 8 },
    retry: { enabled: false },
  });
  const snapshot: {
    compact: Array<{ reason: string; isIdle: boolean; isStreaming: boolean }>;
    settled: number;
    requests: Array<{ blob: string }>;
  } = { compact: [], settled: 0, requests: [] };
  const holder: { session?: { isStreaming: boolean; isIdle: boolean; isCompacting: boolean } } = {};
  const loader = new sdk.DefaultResourceLoader({
    cwd,
    agentDir: cwd,
    settingsManager,
    noExtensions: true,
    noSkills: true,
    noThemes: true,
    noPromptTemplates: true,
    noContextFiles: true,
    extensionFactories: [
      (pi) => {
        pi.on("session_before_compact", (event, ctx) => {
          snapshot.compact.push({
            reason: `${event.reason}:before`,
            isIdle: ctx.isIdle(),
            isStreaming: holder.session?.isStreaming === true,
          });
          const firstKeptEntryId = event.preparation?.firstKeptEntryId;
          if (!firstKeptEntryId) return { cancel: true };
          return { compaction: { summary: "summary of prior work", firstKeptEntryId, tokensBefore: 999 } };
        });
        pi.on("session_compact", (event, ctx) => {
          snapshot.compact.push({
            reason: event.reason,
            isIdle: ctx.isIdle(),
            isStreaming: holder.session?.isStreaming === true,
          });
        });
        pi.on("agent_settled", () => {
          snapshot.settled++;
        });
      },
    ],
  });
  await loader.reload();
  const { session } = await sdk.createAgentSession({
    cwd,
    agentDir: cwd,
    model: faux.getModel() as never,
    modelRuntime,
    noTools: "all",
    settingsManager,
    resourceLoader: loader,
    sessionManager: sdk.SessionManager.inMemory(cwd),
  });
  holder.session = session;
  await session.bindExtensions({ mode: "rpc" });
  faux.setResponses(Array.from({ length: 20 }, () => (context) => {
    snapshot.requests.push({ blob: JSON.stringify(context.messages) });
    return ai.fauxAssistantMessage("ok");
  }));
  return { version, session, snapshot, sdk };
}

describe.each(hosts)("pi host compaction ($label)", (host) => {
  it("manual compact does not settle, isStreaming is false, and a steered terminal is not model-visible until a later triggerTurn", async () => {
    const { session, snapshot } = await makeSession(host);
    for (let i = 0; i < 6; i++) await session.prompt(`PAD ${"x".repeat(200)} #${i}`);
    const settledBefore = snapshot.settled;
    const requestsBefore = snapshot.requests.length;
    await session.compact();
    const compact = snapshot.compact.filter((entry) => entry.reason === "manual");
    expect(compact.length).toBeGreaterThan(0);
    expect(compact.every((entry) => entry.isStreaming === false)).toBe(true);
    expect(compact.every((entry) => entry.isIdle === false)).toBe(true);
    expect(snapshot.settled).toBe(settledBefore);

    await session.sendCustomMessage(terminalNotice("steer-append"), { deliverAs: "steer" });
    expect(snapshot.requests.length).toBe(requestsBefore);
    const inSession = session.messages.some((message) => String((message as { content?: unknown }).content ?? "").includes("terminal:steer-append"));
    expect(inSession).toBe(true);

    await session.sendCustomMessage(terminalNotice("trigger"), { triggerTurn: true });
    const seen = snapshot.requests.slice(requestsBefore).some((request) =>
      request.blob.includes("terminal:steer-append"),
    );
    expect(seen).toBe(true);
    session.dispose();
  });

  it("a turn started before a later append does not put that suffix in the 1.0.0 model request", async () => {
    const { session, snapshot, version } = await makeSession(host);
    const requestsBefore = snapshot.requests.length;
    const first = session.sendCustomMessage(terminalNotice("active-first"), { triggerTurn: true });
    await session.sendCustomMessage(terminalNotice("passive-suffix"), { triggerTurn: false });
    await first;
    const triggerBlobs = snapshot.requests.slice(requestsBefore).map((request) => request.blob);
    expect(triggerBlobs.some((blob) => blob.includes("terminal:active-first"))).toBe(true);
    if (version === "1.0.0") {
      expect(triggerBlobs.some((blob) => blob.includes("terminal:passive-suffix"))).toBe(false);
    }
    session.dispose();
  });

  it("records isStreaming during automatic compaction (threshold/overflow) without assuming a version split", async () => {
    const { session, snapshot, version } = await makeSession(host, { contextWindow: 40 });
    await session.prompt(`PAD ${"x".repeat(200)} #0`);
    await session.prompt(`PAD ${"x".repeat(200)} #1`);
    const auto = snapshot.compact.filter((entry) => entry.reason === "threshold" || entry.reason === "overflow" || entry.reason.endsWith(":before"));
    expect(auto.length).toBeGreaterThan(0);
    const during = snapshot.compact.filter((entry) => entry.reason === "threshold" || entry.reason === "overflow");
    expect(during.length).toBeGreaterThan(0);
    // Measured on dependency 1.0.0 and a local global 1.4.0 only. CI matrix
    // also runs 1.0.4 and latest via test:compat, which does not include this file.
    // compact-host's optional global 1.4 lane is skipped when npm root -g has no SDK.
    const measuredStreamingTrue = version === "1.0.0" || version === "1.4.0";
    if (measuredStreamingTrue) {
      for (const entry of during) {
        expect(entry.isStreaming, `${version} ${entry.reason} isStreaming`).toBe(true);
      }
    }
    session.dispose();
  });
});
