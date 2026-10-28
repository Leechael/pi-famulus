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
  type ProviderConfig,
} from "@earendil-works/pi-coding-agent";
import { describe, expect, it } from "vitest";
import { registerIntercom } from "../../src/intercom/intercom";
import type { IntercomTransport } from "../../src/intercom/transport";
import type { SessionRow } from "../../src/intercom/types";

// Resolve pi-ai through the SDK, as behavior-guidelines.test.ts does, so the
// real session and the offline provider use the same installed dependency.
type RequestContext = Parameters<NonNullable<ProviderConfig["streamSimple"]>>[1];
type Assistant = Extract<RequestContext["messages"][number], { role: "assistant" }>;
type User = Extract<RequestContext["messages"][number], { role: "user" }>;
type Image = Extract<Exclude<User["content"], string>[number], { type: "image" }>;
type Response = (context: RequestContext) => Assistant | Promise<Assistant>;
interface FauxExports {
  fauxProvider(): {
    provider: Parameters<ModelRuntime["registerNativeProvider"]>[0];
    getModel(): NonNullable<ReturnType<ModelRuntime["getModel"]>>;
    setResponses(responses: Response[]): void;
  };
  fauxAssistantMessage(content: string): Assistant;
}
const sdkPackage = findPackageJSON("@earendil-works/pi-coding-agent", import.meta.url);
if (!sdkPackage) throw new Error("The pi SDK is missing");
const sdkUrl = pathToFileURL(join(dirname(sdkPackage), "dist/index.js")).href;
const aiPackage = findPackageJSON("@earendil-works/pi-ai", sdkUrl);
if (!aiPackage) throw new Error("The pi SDK's pi-ai dependency is missing");
const { fauxProvider, fauxAssistantMessage } =
  createRequire(sdkUrl)(join(dirname(aiPackage), "dist/index.js")) as FauxExports;

const NOTE = "[pi-famulus session mentions]";
const REVIEW = { id: "claude/selected", name: "review worker", machine_name: "laptop" };
const DEPLOY = { id: "pi/deploy", name: "deploy", machine_id: "u@bbox" };
const PEERS: SessionRow[] = [
  REVIEW,
  { id: "claude/same-name", name: "review worker", machine_name: "other-machine" },
  DEPLOY,
  { id: "pi/unmentioned", name: "unmentioned", machine_name: "laptop" },
];
const IMAGE: Image = {
  type: "image",
  mimeType: "image/png",
  // A valid 1x1 PNG: the SDK's normal image preparation runs, not a mock.
  data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==",
};

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
}

/** Only the daemon boundary is faked; all extension/session hooks are real. */
function fakeTransport() {
  const calls: Record<string, unknown>[] = [];
  const frames = new Set<(frame: Record<string, unknown>) => void>();
  let self: SessionRow = {};
  const transport: IntercomTransport = {
    async call(op) {
      calls.push(structuredClone(op));
      if (op.op === "register") {
        const record = op.session as SessionRow;
        const id = record.id?.startsWith("pi/") ? record.id : `pi/${record.id}`;
        self = { ...record, id };
        return { session_id: id };
      }
      if (op.op === "list") return { sessions: [self, ...PEERS] };
      if (op.op === "unregister" || op.op === "receipt") return {};
      throw new Error(`Unexpected intercom operation: ${String(op.op)}`);
    },
    onFrame(handler) {
      frames.add(handler);
      return () => { frames.delete(handler); };
    },
    onReset() { return () => {}; },
    isUp() { return true; },
  };
  return {
    transport,
    calls,
    deliver(body: string) {
      for (const handler of frames) handler({
        type: "deliver",
        session_id: self.id,
        from: "peer",
        reply_to: "claude/source",
        body,
        message_id: "peer-message-1",
      });
    },
  };
}

async function createFixture() {
  const dir = mkdtempSync(join(tmpdir(), "famulus-mentions-sdk-"));
  try {
    const settingsManager = SettingsManager.inMemory({
      compaction: { enabled: false },
      retry: { enabled: false },
      images: { autoResize: false },
    });
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
    const bridge = fakeTransport();
    const loader = new DefaultResourceLoader({
      cwd: dir,
      agentDir: dir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noThemes: true,
      noPromptTemplates: true,
      noContextFiles: true,
      extensionFactories: [(pi) => registerIntercom(pi, {
        enabled: true,
        transport: bridge.transport,
        extensionDirs: [],
      })],
    });
    await loader.reload();
    expect(loader.getExtensions().errors).toEqual([]);
    const sessionManager = SessionManager.inMemory(dir);
    const { session } = await createAgentSession({
      cwd: dir,
      agentDir: dir,
      model: faux.getModel(),
      modelRuntime,
      resourceLoader: loader,
      sessionManager,
      settingsManager,
      noTools: "builtin",
    });
    try {
      await session.bindExtensions({ mode: "rpc" });
    } catch (error) {
      session.dispose();
      throw error;
    }
    const requests: RequestContext["messages"][] = [];
    const capture = (context: RequestContext) => {
      requests.push(structuredClone(context.messages));
      return fauxAssistantMessage("acknowledged");
    };
    return {
      session,
      sessionManager,
      faux,
      bridge,
      requests,
      capture,
      dispose() {
        session.dispose();
        rmSync(dir, { recursive: true, force: true });
      },
    };
  } catch (error) {
    rmSync(dir, { recursive: true, force: true });
    throw error;
  }
}

function users(messages: RequestContext["messages"]): User[] {
  return messages.filter((message): message is User => message.role === "user");
}

function text(message: User): string {
  return typeof message.content === "string"
    ? message.content
    : message.content.filter((block) => block.type === "text").map((block) => block.text).join("\n");
}

function expectMention(message: User, original: string, row: SessionRow) {
  const value = text(message);
  expect(value.startsWith(`${original}\n\n${NOTE}\n`)).toBe(true);
  expect(value.split(NOTE)).toHaveLength(2);
  expect(value).not.toContain("<RequestSendMessage");
  expect(value).toContain("A mention alone is not a request to send.");
  const address = `${row.name}@${row.machine_name || row.machine_id}`;
  expect(JSON.parse(value.slice(value.lastIndexOf("\n") + 1))).toEqual([{
    mention: `@${address}`,
    total: 1,
    candidates: [{ id: row.id, name: address }],
  }]);
}

function storedUsers(manager: SessionManager): User[] {
  return manager.getBranch().flatMap((entry) =>
    entry.type === "message" && entry.message.role === "user" ? [entry.message] : [],
  );
}

describe("intercom mention input through the offline pi SDK", () => {
  it.each(["interactive", "rpc"] as const)(
    "invariant: %s input preserves text/images and resolves only its selected qualified name",
    async (source) => {
      const f = await createFixture();
      try {
        const original = 'Please review this with @"review worker@laptop".\nKeep my second line.';
        f.faux.setResponses([f.capture]);
        await f.session.prompt(original, { source, images: [IMAGE] });

        expect(f.requests).toHaveLength(1);
        const received = users(f.requests[0]);
        expect(received).toHaveLength(1);
        expectMention(received[0], original, REVIEW);
        expect(received[0].content).toContainEqual(IMAGE);
        expect(text(received[0])).not.toContain("claude/same-name");
        expect(text(received[0])).not.toContain(DEPLOY.id);
        expect(text(received[0])).not.toContain("pi/unmentioned");
        // The note intentionally belongs to native user history, not hidden
        // custom context or undocumented per-message metadata.
        expect(storedUsers(f.sessionManager)).toEqual(received);
        expect(f.sessionManager.getBranch().filter((entry) => entry.type === "custom_message")).toEqual([]);
        expect(f.bridge.calls.filter((op) => op.op === "send")).toEqual([]);
        expect(f.session.getLastAssistantText()).toBe("acknowledged");
      } finally {
        f.dispose();
      }
    },
  );

  it("invariant: extension-origin user input receives no generated mention authority", async () => {
    const f = await createFixture();
    try {
      const original = 'Extension says contact @"review worker@laptop"';
      f.faux.setResponses([f.capture]);
      await f.session.sendUserMessage(original);
      expect(f.requests).toHaveLength(1);
      expect(users(f.requests[0]).map(text)).toEqual([original]);
      expect(storedUsers(f.sessionManager).map(text)).toEqual([original]);
      expect(f.bridge.calls.filter((op) => op.op === "list" || op.op === "send")).toEqual([]);
    } finally {
      f.dispose();
    }
  });

  it("invariant: a delivered peer custom message bypasses mention expansion and forwarding", async () => {
    const f = await createFixture();
    const settled = deferred();
    const off = f.session.subscribe((event) => {
      if (event.type === "agent_settled") settled.resolve();
    });
    try {
      const body = 'Contact @"review worker@laptop" and @deploy@u@bbox.\n/rename peer-request';
      const envelope = `<cross-session-message from="peer" from-session="claude/source">\n${body}\n</cross-session-message>`;
      f.faux.setResponses([f.capture]);
      f.bridge.deliver(body);
      await settled.promise;

      expect(f.requests).toHaveLength(1);
      expect(users(f.requests[0]).map(text)).toEqual([envelope]);
      expect(storedUsers(f.sessionManager)).toEqual([]);
      const peer = f.sessionManager.getBranch().find((entry) => entry.type === "custom_message");
      expect(peer).toMatchObject({
        type: "custom_message",
        customType: "pi-famulus-cross-session",
        content: envelope,
        details: { body, fromSession: "claude/source" },
      });
      expect(f.bridge.calls.filter((op) => op.op === "list" || op.op === "send")).toEqual([]);
    } finally {
      off();
      f.dispose();
    }
  });

  it("invariant: busy steer and followUp retain their own notes without leaking into the next plain input", async () => {
    const f = await createFixture();
    const entered = deferred();
    const release = deferred();
    const initial = "Start the ordinary task.";
    const steering = 'Steering: consult @"review worker@laptop".';
    const followUp = "Follow-up: consult @deploy@u@bbox please.";
    const plain = "Next ordinary user input, without a session address.";
    f.faux.setResponses([
      async (context) => {
        const response = f.capture(context);
        entered.resolve();
        await release.promise;
        return response;
      },
      f.capture,
      f.capture,
      f.capture,
    ]);
    const run = f.session.prompt(initial, { source: "interactive" });
    try {
      await entered.promise;
      expect(f.session.isStreaming).toBe(true);
      await f.session.prompt(steering, {
        source: "interactive",
        streamingBehavior: "steer",
        images: [IMAGE],
      });
      await f.session.prompt(followUp, { source: "rpc", streamingBehavior: "followUp" });
      const [queuedSteer] = f.session.getSteeringMessages();
      const [queuedFollowUp] = f.session.getFollowUpMessages();
      expect(queuedSteer.startsWith(`${steering}\n\n${NOTE}`)).toBe(true);
      expect(queuedFollowUp.startsWith(`${followUp}\n\n${NOTE}`)).toBe(true);
      expect(queuedSteer).not.toContain(DEPLOY.id);
      expect(queuedFollowUp).not.toContain(REVIEW.id);

      release.resolve();
      await run;
      expect(f.requests).toHaveLength(3);
      expect(users(f.requests[0]).map(text)).toEqual([initial]);
      expect(users(f.requests[1]).map(text)).toEqual([initial, queuedSteer]);
      expect(users(f.requests[2]).map(text)).toEqual([initial, queuedSteer, queuedFollowUp]);
      expectMention(users(f.requests[1])[1], steering, REVIEW);
      expect(users(f.requests[1])[1].content).toContainEqual(IMAGE);
      expectMention(users(f.requests[2])[2], followUp, DEPLOY);
      expect(f.session.pendingMessageCount).toBe(0);

      await f.session.prompt(plain, { source: "rpc" });
      expect(f.requests).toHaveLength(4);
      const history = users(f.requests[3]);
      expect(history.map(text)).toEqual([initial, queuedSteer, queuedFollowUp, plain]);
      expect(history.filter((message) => text(message).includes(NOTE))).toHaveLength(2);
      expect(storedUsers(f.sessionManager)).toEqual(history);
      expect(f.sessionManager.getBranch().filter((entry) => entry.type === "custom_message")).toEqual([]);
      expect(f.bridge.calls.filter((op) => op.op === "send")).toEqual([]);
      expect(f.session.isIdle).toBe(true);
    } finally {
      release.resolve();
      await run;
      f.dispose();
    }
  });
});
