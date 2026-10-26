import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { collectFamulusInfo, formatFamulusInfo, registerFamulusCommand } from "../../src/tui/famulus-command";
import type { ManagerClient } from "../../src/manager-client";

function client(connected: boolean, lastError = ""): ManagerClient {
  return {
    isAvailable: () => connected,
    managerInfo: () => ({ version: "0.1.6+abc", pid: 4242 }),
    lastError: () => lastError,
  } as unknown as ManagerClient;
}

describe("/famulus", () => {
  it("shows the binary path and version, the running manager, and the extension version", async () => {
    const info = await collectFamulusInfo({
      extensionVersion: "0.1.6",
      configPath: "/home/user/.pi/agent/pi-famulus/config.json",
      managerPath: "/opt/bin/pi-famulus",
      describeSearch: () => "unused",
      getClient: () => client(true),
      binaryVersion: async () => "0.1.6+def",
    });
    expect(formatFamulusInfo(info)).toBe(
      [
        "pi-famulus extension 0.1.6",
        "CLI binary: /opt/bin/pi-famulus (0.1.6+def)",
        "Manager: 0.1.6+abc (pid 4242)",
        "Config: /home/user/.pi/agent/pi-famulus/config.json",
      ].join("\n"),
    );
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

  it("edits config.json from the interactive menu, including manager settings", async () => {
    const home = mkdtempSync(join(tmpdir(), "famulus-command-"));
    const configPath = join(home, "config.json");
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
      getClient: () => null,
    });
    const actions = ["Edit configuration", "Close"];
    const titles: string[] = [];
    const notify = vi.fn();
    const ctx = {
      hasUI: true,
      ui: {
        select: vi.fn(async (title: string) => {
          titles.push(title);
          return actions.shift();
        }),
        editor: vi.fn(async () => '{"foregroundBudgetMs":30000,"goneSessionRetention":"12h"}'),
        notify,
      },
    };

    await handler!("", ctx);

    expect(titles[0]).toContain(`Config: ${configPath}`);
    expect(ctx.ui.editor).toHaveBeenCalledWith(`Edit ${configPath}`, "{}\n");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({
      foregroundBudgetMs: 30000,
      goneSessionRetention: "12h",
    });
    expect(notify).toHaveBeenCalledWith(`Saved ${configPath}. Reload the extension to apply extension settings.`, "info");
  });

  it("rejects invalid JSON without writing and can reload from the menu", async () => {
    const configPath = join(mkdtempSync(join(tmpdir(), "famulus-command-")), "config.json");
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
      getClient: () => null,
    });
    const actions = ["Edit configuration", "Reload extension"];
    const notify = vi.fn();
    const reload = vi.fn(async () => {});
    const ctx = {
      hasUI: true,
      ui: {
        select: vi.fn(async () => actions.shift()),
        editor: vi.fn(async () => "not json"),
        notify,
      },
      reload,
    };

    await handler!("", ctx);

    expect(notify.mock.calls[0][0]).toContain("Configuration was not saved:");
    expect(existsSync(configPath)).toBe(false);
    expect(reload).toHaveBeenCalledOnce();
  });
});
