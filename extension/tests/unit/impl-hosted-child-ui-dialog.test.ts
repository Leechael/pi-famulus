/**
 * An extension inside a hosted child can open a dialog
 * (ctx.ui.confirm and friends). The child then waits for an answer. The
 * daemon cancels such a dialog itself only when nobody watches the child
 * (manager/src/hosted.rs on_line, `extension_ui_request`); the parent always
 * watches its children, so the daemon forwards it as an `agent_ui` event.
 * The parent must then tell the model what the child is waiting for.
 *
 * The fake daemon sends that event exactly as the daemon does.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { resetHostedFakes, settleAsync, startHostedExtension } from "./hosted-fakes";
// Load the extension at collection time, outside the first test's timeout (see startHostedExtension).
import "../../src/index";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-ui-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

describe("a dialog inside a hosted child", () => {
  it.each([
    { method: "notify", message: "working", notifyType: "info" },
    { method: "setStatus", statusKey: "progress", statusText: "working" },
    { method: "setWidget", widgetKey: "progress", widgetLines: ["working"] },
    { method: "setTitle", title: "working" },
    { method: "set_editor_text", text: "working" },
  ])("does not claim a child is waiting on fire-and-forget $method", async (request) => {
    const ext = await startHostedExtension({ sessionId: `sess-ui-${request.method}`, cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      const before = ext.pi.sent.length;
      // RpcExtensionUIRequest uses the same envelope for these display-only
      // operations, but rpc-mode.ts creates no dialog promise for them.
      ext.manager.emit({ event: "agent_ui", child_id: childId, request: {
        type: "extension_ui_request", id: "ui_update", ...request,
      } });
      await settleAsync();
      expect(ext.pi.sent.slice(before)).toEqual([]);
    } finally {
      await ext.shutdown();
    }
  });

  it.each(["select", "confirm", "input", "editor"])("wakes the parent for a response-bearing %s dialog", async (method) => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    const ext = await startHostedExtension({ sessionId: "sess-ui", cwd: home });
    try {
      const childId = await ext.spawnChild();
      ext.manager.rpc(childId, { type: "agent_start" });
      const before = ext.pi.sent.length;
      ext.manager.emit({
        event: "agent_ui",
        child_id: childId,
        request: {
          type: "extension_ui_request",
          id: "ui_1",
          method,
          title: "Approve deploy?",
          message: "Deploy build 42 to production?",
        },
      });
      await settleAsync();
      // Long enough for any batching, far short of the first overrun reminder.
      await vi.advanceTimersByTimeAsync(5_000);
      const after = ext.pi.sent.slice(before).map((entry) => entry.message.content ?? "");
      expect({
        wakes: after.length > 0,
        namesChild: after.some((text) => text.includes(childId)),
        namesDialog: after.some((text) => text.includes("Approve deploy?")),
      }).toEqual({ wakes: true, namesChild: true, namesDialog: true });
    } finally {
      vi.useRealTimers();
      await ext.shutdown();
    }
  });
});
