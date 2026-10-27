/**
 * /famulus — which pi-famulus binary this session uses, which versions run,
 * whether messaging reaches its relay, and the settings pages.
 */
import { execFile } from "node:child_process";
import { stripVTControlCharacters } from "node:util";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";
import type { ManagerClient, MessagingStatus } from "../manager-client";
import { SETTINGS_SECTIONS, listTheme, readConfigText, sectionPage, writeConfigText } from "./famulus-settings";
import { loadPiTui, truncateToWidth, wrapTextWithAnsi } from "./pi-tui-load";

export interface FamulusCommandDeps {
  extensionVersion: string;
  configPath: string;
  managerPath: string | null;
  /** Where the binary was looked for, shown when none was found. */
  describeSearch: () => string;
  getClient: () => ManagerClient | null;
  /** Effective plugin opt-in, including the environment disable override. */
  messagingEnabled?: boolean;
  /** `<binary> --version`; null when it cannot be run. */
  binaryVersion?: (path: string) => Promise<string | null>;
}

export interface FamulusInfo {
  extensionVersion: string;
  configPath: string;
  managerPath: string | null;
  searched: string;
  binaryVersion: string | null;
  daemon: { version: string; pid: number } | null;
  daemonError: string;
  /** Null when the manager is not connected or did not report it. */
  messaging: MessagingStatus | null;
}

const RELAY_STATES: Record<string, string> = {
  disabled: "disabled",
  connected: "connected",
  disconnected: "disconnected",
  local_only: "local-only",
};

function singleLine(text: string): string {
  return stripVTControlCharacters(text).replace(/\s+/g, " ").trim();
}

export function formatMessaging(info: FamulusInfo): string {
  const m = info.messaging;
  if (!m) return info.daemon ? "not reported by this manager" : "unknown (manager not connected)";
  const state = RELAY_STATES[m.state] ?? "unavailable";
  const label = m.name && RELAY_STATES[m.state] ? `${state} · name ${singleLine(m.name)}` : state;
  const error = singleLine(m.error ?? "");
  return `${label}${error ? ` (${error})` : ""}`;
}

export function formatFamulusInfo(info: FamulusInfo): string {
  const binary = info.managerPath
    ? `${info.managerPath}${info.binaryVersion ? ` (${info.binaryVersion})` : ""}`
    : `not found; looked in: ${info.searched}`;
  const manager = info.daemon
    ? `${info.daemon.version} (pid ${info.daemon.pid})`
    : `not connected${info.daemonError ? ` (${info.daemonError})` : ""}`;
  return [
    `pi-famulus extension ${info.extensionVersion}`,
    `CLI binary: ${binary}`,
    `Manager: ${manager}`,
    `Messaging: ${formatMessaging(info)}`,
    `Config: ${info.configPath}`,
  ].join("\n");
}

export function readBinaryVersion(path: string): Promise<string | null> {
  return new Promise((resolve) => {
    execFile(path, ["--version"], { timeout: 2000, windowsHide: true }, (err, stdout) => {
      if (err) return resolve(null);
      // "pi-famulus 0.1.6+<sha>"
      resolve(stdout.trim().replace(/^pi-famulus\s+/, "") || null);
    });
  });
}

const STATUS_TIMEOUT_MS = 2000;

async function readMessaging(client: ManagerClient): Promise<MessagingStatus | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), STATUS_TIMEOUT_MS);
  });
  try {
    const status = client.status().then((s) => s.messaging ?? null, () => null);
    return await Promise.race([status, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

export async function collectFamulusInfo(deps: FamulusCommandDeps): Promise<FamulusInfo> {
  const client = deps.getClient();
  const connected = client?.isAvailable() ?? false;
  return {
    extensionVersion: deps.extensionVersion,
    configPath: deps.configPath,
    managerPath: deps.managerPath,
    searched: deps.managerPath ? "" : deps.describeSearch(),
    binaryVersion: deps.managerPath ? await (deps.binaryVersion ?? readBinaryVersion)(deps.managerPath) : null,
    daemon: connected ? client?.managerInfo() ?? null : null,
    daemonError: connected ? "" : client?.lastError() ?? "",
    messaging: deps.messagingEnabled === false
      ? { state: "disabled", name: "" }
      : connected && client ? await readMessaging(client) : null,
  };
}

type OverviewChoice = "edit-raw" | "reload" | undefined;

/** Status first, then one row per settings section and the two actions. */
function showOverview(ctx: ExtensionCommandContext, info: FamulusInfo, configPath: string): Promise<OverviewChoice> {
  const piTui = loadPiTui();
  if (!piTui) {
    ctx.ui.notify(formatFamulusInfo(info), "info");
    return Promise.resolve(undefined);
  }
  return ctx.ui.custom<OverviewChoice>((tui, theme, _kb, done) => {
    let notice = "";
    const deps = {
      tui: piTui,
      theme,
      configPath,
      requestRender: () => tui.requestRender(),
      notice: (text: string, error = false) => {
        notice = theme.fg(error ? "error" : "success", text);
      },
    };
    const items = [
      ...SETTINGS_SECTIONS.map((section) => ({
        id: `section:${section.id}`,
        label: section.label,
        description: `${section.specs.length} settings · ${section.applies}`,
        currentValue: "›",
        submenu: (_current: string, close: (value?: string) => void) => sectionPage(deps, section, () => close()),
      })),
      {
        id: "edit-raw",
        label: "Edit config.json",
        description: "The whole file in the editor, for webhook endpoints and anything not listed",
        currentValue: "",
        values: [""],
      },
      { id: "reload", label: "Reload extension", description: "Apply extension settings now", currentValue: "", values: [""] },
    ];
    const list = new piTui.SettingsList(
      items,
      10,
      listTheme(theme),
      (id) => {
        if (id === "edit-raw" || id === "reload") done(id);
      },
      () => done(undefined),
    );
    return {
      render(width: number) {
        const status = formatFamulusInfo({
          ...info,
          messaging: info.messaging ? { ...info.messaging, error: "" } : null,
        }).split("\n");
        const lines = [theme.fg("accent", status[0]), ...status.slice(1).map((line) => {
          const at = line.indexOf(": ");
          return `  ${theme.fg("dim", line.slice(0, at + 1).padEnd(12))}${line.slice(at + 2)}`;
        })];
        const error = singleLine(info.messaging?.error ?? "");
        if (error) lines.push(...wrapTextWithAnsi(theme.fg("error", `Messaging error: ${error}`), width));
        lines.push("", ...list.render(width));
        if (notice) lines.push("", notice);
        return lines.map((line) => truncateToWidth(line, width));
      },
      invalidate: () => list.invalidate(),
      handleInput(data: string) {
        notice = "";
        list.handleInput(data);
        tui.requestRender();
      },
    };
  });
}

export function registerFamulusCommand(pi: ExtensionAPI, deps: FamulusCommandDeps): void {
  pi.registerCommand("famulus", {
    description: "Show pi-famulus status and manage its configuration",
    handler: async (_args, ctx) => {
      if (!ctx.hasUI) {
        ctx.ui.notify(formatFamulusInfo(await collectFamulusInfo(deps)), "info");
        return;
      }
      while (true) {
        const action = await showOverview(ctx, await collectFamulusInfo(deps), deps.configPath);
        if (action === "reload") {
          await ctx.reload();
          return;
        }
        if (action !== "edit-raw") return;
        let current: string;
        try {
          current = readConfigText(deps.configPath);
        } catch (error) {
          ctx.ui.notify(`Cannot read ${deps.configPath}: ${(error as Error).message}`, "error");
          continue;
        }
        const edited = await ctx.ui.editor(`Edit ${deps.configPath}`, current);
        if (edited === undefined) continue;
        try {
          writeConfigText(deps.configPath, edited);
          ctx.ui.notify(
            `Saved ${deps.configPath}. Reload the extension for extension settings; restart the manager daemon for messaging settings.`,
            "info",
          );
        } catch (error) {
          ctx.ui.notify(`Configuration was not saved: ${(error as Error).message}`, "error");
        }
      }
    },
  });
}
