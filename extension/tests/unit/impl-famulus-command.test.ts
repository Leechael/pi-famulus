import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { collectFamulusInfo, formatFamulusInfo, registerFamulusCommand } from "../../src/tui/famulus-command";
import type { ManagerClient, MessagingStatus } from "../../src/manager-client";

function client(connected: boolean, lastError = "", messaging?: MessagingStatus | Error): ManagerClient {
  return {
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
  drive: (screen: Component, round: number) => void,
  extra: { editor?: (title: string, text: string) => Promise<string | undefined>; messaging?: MessagingStatus } = {},
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
    getClient: () => client(true, "", extra.messaging ?? { state: "connected", name: "castle" }),
  });
  const notify = vi.fn();
  const reload = vi.fn(async () => {});
  const editor = vi.fn(extra.editor ?? (async () => undefined));
  let round = 0;
  const ctx = {
    hasUI: true,
    ui: {
      custom: (factory: Function) =>
        new Promise((resolve) => {
          const screen = factory({ requestRender() {} }, plain, {}, resolve) as Component;
          drive(screen, round++);
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
    expect(screens[0]).toMatch(/token\s+set/);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ messaging: { token: "s3cret-token" } });
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
