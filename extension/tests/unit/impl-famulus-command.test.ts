import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { collectFamulusInfo, formatFamulusInfo, registerFamulusCommand, type FamulusCommandDeps } from "../../src/tui/famulus-command";
import { ManagerError, type ManagerClient, type MessagingStatus } from "../../src/manager-client";
import { visibleWidth } from "../../src/tui/pi-tui-load";

function client(
  connected: boolean,
  lastError = "",
  messaging?: MessagingStatus | Error,
  messagingReconnect: () => Promise<void> = async () => {},
): ManagerClient {
  return {
    messagingReconnect,
    isAvailable: () => connected,
    managerInfo: () => ({ version: "0.1.6+abc", pid: 4242 }),
    lastError: () => lastError,
    status: async () => {
      if (messaging instanceof Error) throw messaging;
      return { sessions: [], ...(messaging ? { messaging } : {}) };
    },
  } as unknown as ManagerClient;
}

const plain = { fg: (_color: string, text: string) => text };
const KEY = { up: "\x1b[A", down: "\x1b[B", enter: "\r", esc: "\x1b" };

interface Component {
  render(width: number): string[];
  handleInput(data: string): void;
}

/** Runs /famulus with a TUI: `drive` gets each overview the loop opens. */
async function runInteractive(
  configPath: string,
  drive: (screen: Component, round: number) => void | Promise<void>,
  extra: {
    editor?: (title: string, text: string) => Promise<string | undefined>;
    messaging?: MessagingStatus | (() => MessagingStatus);
    messagingReconnect?: () => Promise<void>;
    enrollMessagingToken?: FamulusCommandDeps["enrollMessagingToken"];
  } = {},
) {
  let handler: ((args: string, ctx: unknown) => Promise<void>) | undefined;
  const pi = {
    registerCommand: (_name: string, spec: { handler: typeof handler }) => {
      handler = spec.handler;
    },
  };
  registerFamulusCommand(pi as never, {
    extensionVersion: "0.2.1",
    configPath,
    managerPath: null,
    describeSearch: () => "PATH",
    getClient: () => client(
      true, "",
      (typeof extra.messaging === "function" ? extra.messaging() : extra.messaging) ?? { state: "connected", name: "castle" },
      extra.messagingReconnect,
    ),
    enrollMessagingToken: extra.enrollMessagingToken,
  });
  const notify = vi.fn();
  const reload = vi.fn(async () => {});
  const editor = vi.fn(extra.editor ?? (async () => undefined));
  let round = 0;
  const ctx = {
    hasUI: true,
    ui: {
      custom: (factory: Function) =>
        new Promise((resolve, reject) => {
          const screen = factory({ requestRender() {} }, plain, {}, resolve) as Component;
          Promise.resolve().then(() => drive(screen, round++)).catch(reject);
        }),
      editor,
      notify,
    },
    reload,
  };
  await handler!("", ctx);
  return { notify, reload, editor };
}

function freshConfig(content?: string): string {
  const path = join(mkdtempSync(join(tmpdir(), "famulus-command-")), "config.json");
  if (content !== undefined) writeFileSync(path, content);
  return path;
}

function press(screen: Component, ...keys: string[]) {
  for (const key of keys) screen.handleInput(key);
}

function type(screen: Component, text: string) {
  for (const ch of text) screen.handleInput(ch);
}

function openMessagingToken(screen: Component) {
  press(screen, KEY.down, KEY.down, KEY.down, KEY.enter); // Messaging
  press(screen, KEY.down, KEY.down, KEY.down, KEY.enter); // token
  expect(screen.render(160).join("\n")).toContain("messaging.token");
}

describe("/famulus status", () => {
  it("shows the binary path and version, the running manager, messaging, and the extension version", async () => {
    const info = await collectFamulusInfo({
      extensionVersion: "0.1.6",
      configPath: "/home/user/.pi/agent/pi-famulus/config.json",
      managerPath: "/opt/bin/pi-famulus",
      describeSearch: () => "unused",
      getClient: () => client(true, "", { state: "connected", name: "castle" }),
      binaryVersion: async () => "0.1.6+def",
    });
    expect(formatFamulusInfo(info)).toBe(
      [
        "pi-famulus extension 0.1.6",
        "CLI binary: /opt/bin/pi-famulus (0.1.6+def)",
        "Manager: 0.1.6+abc (pid 4242)",
        "Messaging: connected · name castle",
        "Config: /home/user/.pi/agent/pi-famulus/config.json",
      ].join("\n"),
    );
  });

  it.each([
    [{ state: "disabled", name: "" }, "Messaging: disabled"],
    [{ state: "disconnected", name: "castle", error: "relay TLS certificate rejected" }, "Messaging: disconnected · name castle (relay TLS certificate rejected)"],
    [{ state: "disconnected", name: "castle" }, "Messaging: disconnected · name castle"],
    [{ state: "local_only", name: "castle" }, "Messaging: local-only · name castle"],
    [{ state: "unavailable", name: "", error: "disabled by PI_FAMULUS_NO_INTERCOM" }, "Messaging: unavailable (disabled by PI_FAMULUS_NO_INTERCOM)"],
    [undefined, "Messaging: not reported by this manager"],
    [new Error("rejected"), "Messaging: not reported by this manager"],
  ])("reports the relay link %#", async (messaging, line) => {
    const info = await collectFamulusInfo({
      extensionVersion: "0.1.6",
      configPath: "/c.json",
      managerPath: null,
      describeSearch: () => "PATH",
      getClient: () => client(true, "", messaging as MessagingStatus | Error | undefined),
    });
    expect(formatFamulusInfo(info).split("\n")).toContain(line);
  });

  it("reports a disabled plugin even when the daemon has messaging enabled", async () => {
    const manager = client(true, "", { state: "connected", name: "castle" });
    const status = vi.spyOn(manager, "status");
    const info = await collectFamulusInfo({
      extensionVersion: "0.2.1", configPath: "/c.json", managerPath: null,
      describeSearch: () => "PATH", getClient: () => manager, messagingEnabled: false,
    });
    expect(formatFamulusInfo(info)).toContain("Messaging: disabled");
    expect(status).not.toHaveBeenCalled();
  });

  it("says where it looked when no binary is found and why the manager is not connected", async () => {
    const info = await collectFamulusInfo({
      extensionVersion: "0.1.6",
      configPath: "/home/user/.pi/agent/pi-famulus/config.json",
      managerPath: null,
      describeSearch: () => "config, PATH",
      getClient: () => client(false, "spawn failed"),
      binaryVersion: async () => {
        throw new Error("must not run without a binary");
      },
    });
    const text = formatFamulusInfo(info);
    expect(text).toContain("CLI binary: not found; looked in: config, PATH");
    expect(text).toContain("Manager: not connected (spawn failed)");
    expect(text).toContain("Messaging: unknown (manager not connected)");
    expect(text).toContain("Config: /home/user/.pi/agent/pi-famulus/config.json");
  });

  it("shows information without a menu when interactive UI is unavailable", async () => {
    let handler: ((args: string, ctx: unknown) => Promise<void>) | undefined;
    const pi = {
      registerCommand: (_name: string, spec: { handler: typeof handler }) => {
        handler = spec.handler;
      },
    };
    const notify = vi.fn();
    registerFamulusCommand(pi as never, {
      extensionVersion: "0.2.1",
      configPath: "/tmp/famulus/config.json",
      managerPath: null,
      describeSearch: () => "PATH",
      getClient: () => null,
    });

    await handler!("", { hasUI: false, ui: { notify } });

    expect(notify).toHaveBeenCalledOnce();
    expect(notify.mock.calls[0][0]).toContain("Config: /tmp/famulus/config.json");
  });
});

describe("/famulus settings", () => {
  it("opens on the status, then lists the settings sections and actions", async () => {
    const configPath = freshConfig();
    let shown = "";
    await runInteractive(configPath, (screen) => {
      shown = screen.render(100).join("\n");
      press(screen, KEY.esc);
    });
    const at = (text: string) => shown.indexOf(text);
    expect(at("Messaging:  connected · name castle")).toBeGreaterThan(0);
    for (const row of ["Extension", "Subagents", "Manager daemon", "Messaging", "Edit config.json", "Reload extension"]) {
      expect(shown).toContain(row);
    }
    expect(at("Messaging:")).toBeLessThan(at("Edit config.json"));
  });

  it("wraps the concrete relay HTTP error at narrow widths without treating body newlines as UI rows", async () => {
    let lines: string[] = [];
    await runInteractive(freshConfig(), (screen) => {
      lines = screen.render(42);
      press(screen, KEY.esc);
    }, { messaging: {
      state: "disconnected", name: "a-device-name-long-enough-to-fill-the-summary",
      error: 'relay websocket connection failed:\nHTTP 401\r\n{"error":"device credential required"}',
    } });
    const shown = lines.join("\n");
    expect(shown).toContain("Messaging error:");
    expect(shown).toContain("HTTP 401");
    expect(shown.replace(/\s+/g, " ")).toContain('"device credential required"');
    expect(lines.every((line) => visibleWidth(line) <= 42)).toBe(true);
  });

  it("edits one number in place, keeping other keys, and says how it applies", async () => {
    const configPath = freshConfig('{"goneSessionRetention":"12h"}\n');
    let after = "";
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.enter); // Extension
      press(screen, KEY.enter); // foregroundBudgetMs
      type(screen, "30000");
      press(screen, KEY.enter);
      after = screen.render(100).join("\n");
      press(screen, KEY.esc, KEY.esc);
    });
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ goneSessionRetention: "12h", foregroundBudgetMs: 30000 });
    expect(after).toContain("Saved foregroundBudgetMs; reload the extension to apply.");
    expect(after).toMatch(/foregroundBudgetMs\s+30000/);
  });

  it("rejects a bad value without writing and keeps the editor open", async () => {
    const configPath = freshConfig();
    let after = "";
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.enter, KEY.enter);
      type(screen, "soon");
      press(screen, KEY.enter);
      after = screen.render(100).join("\n");
      press(screen, KEY.esc, KEY.esc, KEY.esc);
    });
    expect(after).toContain("Enter a whole number");
    expect(existsSync(configPath)).toBe(false);
  });

  it("clears a key back to its default when the value is emptied", async () => {
    const configPath = freshConfig('{"foregroundBudgetMs":30000}\n');
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.enter, KEY.enter);
      for (let i = 0; i < 5; i++) press(screen, "\x7f");
      press(screen, KEY.enter, KEY.esc, KEY.esc);
    });
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({});
  });

  it("defaults messaging to disabled and explains both apply steps when enabled", async () => {
    const configPath = freshConfig();
    let section = "";
    let saved = "";
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.down, KEY.down, KEY.down, KEY.enter);
      section = screen.render(160).join("\n");
      press(screen, KEY.enter);
      saved = screen.render(160).join("\n");
      press(screen, KEY.esc, KEY.esc);
    });
    expect(section).toMatch(/enabled\s+false/);
    expect(saved).toContain("reload the extension");
    expect(saved).toContain("restart the manager daemon");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { enabled: true } });
  });

  it("toggles a messaging flag into the messaging object and shows the hostname as the default name", async () => {
    const configPath = freshConfig();
    let section = "";
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.down, KEY.down, KEY.down, KEY.enter); // Messaging
      section = screen.render(120).join("\n");
      press(screen, KEY.down, KEY.down, KEY.down, KEY.down, KEY.enter); // localOnly: false → true
      press(screen, KEY.esc, KEY.esc);
    });
    expect(section).toMatch(new RegExp(`name\\s+${hostname().replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`));
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { localOnly: true } });
  });

  it("shows enrollment on the token row instead of 'not set'", async () => {
    const rows: string[] = [];
    for (const messaging of [
      { state: "connected", name: "castle", device_id: "0123456789abcdef" },
      { state: "disconnected", name: "castle", device_id: "" },
      { state: "disconnected", name: "castle" },
    ]) {
      await runInteractive(freshConfig(), (screen) => {
        press(screen, KEY.down, KEY.down, KEY.down, KEY.enter, KEY.down, KEY.down, KEY.down);
        rows.push(screen.render(200).join("\n"));
        press(screen, KEY.esc, KEY.esc);
      }, { messaging });
    }
    expect(rows[0]).toMatch(/token\s+enrolled · device 01234567\b/);
    expect(rows[0]).not.toContain("89abcdef");
    expect(rows[0]).toContain("Entering a token re-enrolls");
    for (const row of rows.slice(1)) {
      expect(row).toMatch(/token\s+not enrolled/);
      expect(row).not.toContain("not set");
      expect(row).toContain("Entering a token enrolls");
    }
  });

  it("never shows a stored secret", async () => {
    const configPath = freshConfig('{"messaging":{"token":"s3cret-token"}}\n');
    let screens: string[] = [];
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.down, KEY.down, KEY.down, KEY.enter, KEY.down, KEY.down, KEY.down);
      screens.push(screen.render(120).join("\n"));
      press(screen, KEY.enter);
      screens.push(screen.render(120).join("\n"));
      press(screen, KEY.enter); // empty keeps the token
      press(screen, KEY.esc, KEY.esc);
    });
    expect(screens.join("\n")).not.toContain("s3cret-token");
    expect(screens[0]).toMatch(/token\s+not enrolled/);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { token: "s3cret-token" } });
  });

  it("exchanges a token immediately, shows pending state, and ignores duplicate submissions", async () => {
    const configPath = freshConfig('{"foregroundBudgetMs":12345}\n');
    const original = readFileSync(configPath, "utf8");
    const token = "pending-enrollment-secret";
    let finish!: () => void;
    const enroll = vi.fn(() => new Promise<void>((resolve) => { finish = resolve; }));
    const { notify } = await runInteractive(configPath, async (screen) => {
      openMessagingToken(screen);
      type(screen, token);
      press(screen, KEY.enter);
      await vi.waitFor(() => expect(enroll).toHaveBeenCalledWith(token));
      expect(screen.render(160).join("\n")).toContain("Exchanging token...");
      expect(readFileSync(configPath, "utf8")).toBe(original);
      press(screen, KEY.enter, KEY.enter);
      expect(enroll).toHaveBeenCalledOnce();
      finish();
      await vi.waitFor(() => expect(screen.render(160).join("\n")).toContain("Device enrolled"));
      const saved = screen.render(160).join("\n");
      expect(saved).toMatch(/enabled\s+false/);
      expect(saved).not.toContain(token);
      expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ foregroundBudgetMs: 12345 });
      press(screen, KEY.esc, KEY.esc);
    }, { enrollMessagingToken: enroll });
    expect(JSON.stringify(notify.mock.calls)).not.toContain(token);
  });

  it.each([undefined, false, true])("removes a legacy token after enrollment without changing enabled=%s or unrelated settings", async (enabled) => {
    const messaging = {
      ...(enabled === undefined ? {} : { enabled }),
      token: "legacy-device-token",
      name: "my-device",
      remoteMessages: "refuse",
    };
    const configPath = freshConfig(JSON.stringify({ foregroundBudgetMs: 9000, messaging }));
    const enroll = vi.fn(async (_token: string) => {});
    await runInteractive(configPath, async (screen) => {
      openMessagingToken(screen);
      type(screen, "new-enrollment-token");
      press(screen, KEY.enter);
      await vi.waitFor(() => expect(screen.render(160).join("\n")).toContain("Device enrolled"));
      expect(enroll).toHaveBeenCalledExactlyOnceWith("new-enrollment-token");
      expect(screen.render(160).join("\n")).not.toContain("new-enrollment-token");
      press(screen, KEY.esc, KEY.esc);
    }, { enrollMessagingToken: enroll });
    const { token: _legacy, ...remaining } = messaging;
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ foregroundBudgetMs: 9000, messaging: remaining });
  });

  it("keeps failed enrollment editable, preserves existing files, redacts notices, and allows retry", async () => {
    const configPath = freshConfig('{"messaging":{"enabled":false,"token":"existing-token"},"logLevel":"debug"}\n');
    const credentialPath = join(configPath, "..", "device.json");
    writeFileSync(credentialPath, '{"credential":"existing-private-credential"}\n', { mode: 0o600 });
    const originalConfig = readFileSync(configPath, "utf8");
    const originalCredential = readFileSync(credentialPath, "utf8");
    const token = "rejected-enrollment-secret";
    const enroll = vi.fn<(token: string) => Promise<void>>()
      .mockRejectedValueOnce(new Error(`HTTP 401: fixture enrollment rejected for ${token}`))
      .mockResolvedValueOnce(undefined);
    const { notify } = await runInteractive(configPath, async (screen) => {
      openMessagingToken(screen);
      type(screen, token);
      press(screen, KEY.enter);
      await vi.waitFor(() => expect(screen.render(200).join("\n")).toContain("HTTP 401"));
      const lines = screen.render(200);
      expect(lines.join("\n")).toContain("fixture enrollment rejected");
      expect(lines.join("\n")).toContain("messaging.token");
      expect(lines.filter((line) => line.includes("HTTP 401")).join("\n")).not.toContain(token);
      expect(readFileSync(configPath, "utf8")).toBe(originalConfig);
      expect(readFileSync(credentialPath, "utf8")).toBe(originalCredential);
      press(screen, "\x15"); // Clear the rejected token before retrying.
      type(screen, "retry-enrollment-token");
      press(screen, KEY.enter);
      await vi.waitFor(() => expect(screen.render(160).join("\n")).toContain("Device enrolled"));
      expect(enroll.mock.calls).toEqual([[token], ["retry-enrollment-token"]]);
      press(screen, KEY.esc, KEY.esc);
    }, { enrollMessagingToken: enroll });
    expect(JSON.stringify(notify.mock.calls)).not.toContain(token);
    expect(JSON.stringify(notify.mock.calls)).not.toContain("retry-enrollment-token");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { enabled: false }, logLevel: "debug" });
  });

  it.each(["blank", "cancel"])("does not enroll or write configuration on %s token submission", async (action) => {
    const configPath = freshConfig('{"messaging":{"token":"existing-token","enabled":false}}\n');
    const original = readFileSync(configPath, "utf8");
    const enroll = vi.fn(async (_token: string) => {});
    await runInteractive(configPath, async (screen) => {
      openMessagingToken(screen);
      if (action === "cancel") {
        type(screen, "cancelled-enrollment-token");
        press(screen, KEY.esc);
      } else {
        type(screen, "   ");
        press(screen, KEY.enter);
      }
      await Promise.resolve();
      expect(enroll).not.toHaveBeenCalled();
      expect(readFileSync(configPath, "utf8")).toBe(original);
      press(screen, KEY.esc, KEY.esc);
    }, { enrollMessagingToken: enroll });
  });

  it("refuses to change a key when config.json is not valid JSON", async () => {
    const configPath = freshConfig("{broken");
    let after = "";
    await runInteractive(configPath, (screen) => {
      press(screen, KEY.enter, KEY.enter);
      type(screen, "30000");
      press(screen, KEY.enter);
      after = screen.render(100).join("\n");
      press(screen, KEY.esc, KEY.esc, KEY.esc);
    });
    expect(after).toContain("foregroundBudgetMs was not saved");
    expect(readFileSync(configPath, "utf8")).toBe("{broken");
  });

  it("edits the whole file from the overview and comes back to it", async () => {
    const configPath = freshConfig();
    const { editor, notify } = await runInteractive(
      configPath,
      (screen, round) => {
        if (round === 0) press(screen, KEY.down, KEY.down, KEY.down, KEY.down, KEY.enter);
        else press(screen, KEY.esc);
      },
      { editor: async () => '{"messaging":{"endpoints":[]}}' },
    );
    expect(editor).toHaveBeenCalledWith(`Edit ${configPath}`, "{}\n");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { endpoints: [] } });
    expect(notify.mock.calls[0][0]).toContain("restart the manager daemon for messaging settings");
  });

  it("rejects invalid JSON from the whole-file editor without writing", async () => {
    const configPath = freshConfig();
    const { notify } = await runInteractive(
      configPath,
      (screen, round) => {
        if (round === 0) press(screen, KEY.down, KEY.down, KEY.down, KEY.down, KEY.enter);
        else press(screen, KEY.esc);
      },
      { editor: async () => "not json" },
    );
    expect(notify.mock.calls[0][0]).toContain("Configuration was not saved:");
    expect(existsSync(configPath)).toBe(false);
  });

  it("reloads the extension from the overview", async () => {
    const { reload } = await runInteractive(freshConfig(), (screen) => press(screen, KEY.up, KEY.enter));
    expect(reload).toHaveBeenCalledOnce();
  });
});

describe("/famulus messaging status and reconnect", () => {
  const down = (extra: Partial<MessagingStatus> = {}): MessagingStatus => ({
    state: "disconnected", name: "castle", error: "IO error: Connection reset by peer (os error 104)", ...extra,
  });

  it("states the link and its retry countdown once, and the error once", async () => {
    vi.spyOn(Date, "now").mockReturnValue(5_000_000);
    let shown = "";
    await runInteractive(freshConfig(), (screen) => {
      shown = screen.render(120).join("\n");
      press(screen, KEY.esc);
    }, { messaging: down({ retry_at_ms: 5_012_000 }) });
    vi.restoreAllMocks();
    expect(shown).toContain("Messaging:  disconnected · name castle · retry in 12s");
    expect(shown.match(/os error 104/g)).toHaveLength(1);
    expect(shown).toContain("Messaging error: IO error");
  });

  it("offers reconnect only while the relay is disconnected or unavailable", async () => {
    const offered: Record<string, boolean> = {};
    for (const state of ["connected", "disconnected", "unavailable", "local_only", "disabled"]) {
      await runInteractive(freshConfig(), (screen) => {
        offered[state] = screen.render(120).join("\n").includes("Reconnect messaging now");
        press(screen, KEY.esc);
      }, { messaging: { state, name: "castle" } });
    }
    expect(offered).toEqual({ connected: false, disconnected: true, unavailable: true, local_only: false, disabled: false });
  });

  it("requests a reconnect, reports it, and shows the refreshed status", async () => {
    let state = "disconnected";
    const reconnect = vi.fn(async () => { state = "connected"; });
    const screens: string[] = [];
    const { notify } = await runInteractive(freshConfig(), (screen, round) => {
      screens.push(screen.render(120).join("\n"));
      if (round === 0) press(screen, KEY.down, KEY.down, KEY.down, KEY.down, KEY.enter);
      else press(screen, KEY.esc);
    }, { messaging: () => ({ state, name: "castle" }), messagingReconnect: reconnect });
    expect(reconnect).toHaveBeenCalledOnce();
    expect(notify).toHaveBeenCalledWith(expect.stringContaining("Reconnect requested"), "info");
    expect(screens[0]).toContain("disconnected");
    expect(screens[1]).toContain("Messaging:  connected");
    expect(screens[1]).not.toContain("Reconnect messaging now");
  });

  it.each([
    [new ManagerError("E_BAD_REQUEST", "unknown variant `messaging_reconnect`, expected one of `status`"), "manager is too old to reconnect on demand; restart it"],
    [new ManagerError("E_BAD_REQUEST", "messaging is disabled"), "messaging is disabled"],
  ])("reports a failed reconnect as an error (%#)", async (error, text) => {
    const { notify } = await runInteractive(freshConfig(), (screen, round) => {
      if (round === 0) press(screen, KEY.down, KEY.down, KEY.down, KEY.down, KEY.enter);
      else press(screen, KEY.esc);
    }, { messaging: down(), messagingReconnect: async () => { throw error; } });
    expect(notify).toHaveBeenCalledWith(expect.stringContaining(text), "error");
  });
});
