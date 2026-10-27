/**
 * /famulus — which pi-famulus binary this session uses, which versions run,
 * whether messaging reaches its relay, and the settings pages.
 */
import { execFile } from "node:child_process";
import { dirname } from "node:path";
import { stripVTControlCharacters } from "node:util";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";
import { ManagerError, type ManagerClient, type MessagingStatus } from "../manager-client";
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
  /** Exchange an enrollment token using the current messaging configuration. */
  enrollMessagingToken?: (token: string) => Promise<void>;
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
  let label = m.name && RELAY_STATES[m.state] ? `${state} · name ${singleLine(m.name)}` : state;
  if (m.state === "disconnected" && typeof m.retry_at_ms === "number") {
    const seconds = Math.ceil((m.retry_at_ms - Date.now()) / 1000);
    label += seconds > 0 ? ` · retry in ${seconds}s` : " · retrying";
  }
  const error = singleLine(m.error ?? "");
  const count = m.conflicts?.length ?? 0;
  const conflicts = count ? ` · ${count} session${count === 1 ? "" : "s"} unpublished` : "";
  return `${label}${conflicts}${error ? ` (${error})` : ""}`;
}

/** Why some local sessions are invisible to peers, one entry per session. */
export function formatConflicts(m: MessagingStatus | null): string {
  if (!m?.conflicts?.length) return "";
  const rows = m.conflicts.map((c) => {
    const id = singleLine(c.id);
    const name = singleLine(c.name);
    const who = name && name !== id ? `${name} (${id})` : id;
    const owner = singleLine(c.owner);
    return owner ? `${who} owned by ${owner}` : `${who} not listed`;
  });
  return `Unpublished: the relay assigns these sessions to another device, so peers cannot reach them until they restart: ${rows.join("; ")}`;
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

export function exchangeMessagingToken(managerPath: string | null, configPath: string, token: string): Promise<void> {
  if (!managerPath) return Promise.reject(new Error("pi-famulus binary is unavailable."));
  return new Promise((resolve, reject) => {
    execFile(managerPath, ["--home", dirname(configPath), "intercom", "enroll", "--token", token],
      { timeout: 25_000, windowsHide: true }, (error, _stdout, stderr) => {
        if (error) reject(new Error(stderr.trim() || `Device enrollment failed (${error.code ?? "command failed"}).`));
        else resolve();
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

type OverviewChoice = "edit-raw" | "reload" | "reconnect" | undefined;

/** Whether "Reconnect messaging now" makes sense for what the manager reported. */
const canReconnect = (m: MessagingStatus | null) => m?.state === "disconnected" || m?.state === "unavailable";

/** Ask the manager to retry the relay now; the notice text says how that went. */
export async function requestMessagingReconnect(client: ManagerClient | null): Promise<{ text: string; error: boolean }> {
  if (!client?.isAvailable()) return { text: "The manager is not connected.", error: true };
  try {
    await client.messagingReconnect();
    return { text: "Reconnect requested; the status refreshes below.", error: false };
  } catch (error) {
    const message = (error as Error).message;
    // An older daemon rejects the request type itself.
    if (error instanceof ManagerError && error.code === "E_BAD_REQUEST" && /unknown|unrecognized|unsupported/i.test(message)) {
      return { text: "The manager is too old to reconnect on demand; restart it.", error: true };
    }
    return { text: `Reconnect failed: ${singleLine(message)}`, error: true };
  }
}

/** Status first, then one row per settings section and the two actions. */
function showOverview(
  ctx: ExtensionCommandContext,
  info: FamulusInfo,
  configPath: string,
  enrollMessagingToken: (token: string) => Promise<void>,
): Promise<OverviewChoice> {
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
      messaging: info.messaging,
      enrollMessagingToken,
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
      ...(canReconnect(info.messaging) ? [{
        id: "reconnect",
        label: "Reconnect messaging now",
        description: "Retry the relay connection immediately instead of waiting for the next attempt",
        currentValue: "",
        values: [""],
      }] : []),
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
        if (id === "edit-raw" || id === "reload" || id === "reconnect") done(id);
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
        const conflicts = formatConflicts(info.messaging);
        if (conflicts) lines.push(...wrapTextWithAnsi(theme.fg("warning", conflicts), width));
        lines.push("", ...list.render(width));
        if (notice) lines.push("", ...wrapTextWithAnsi(notice, width));
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
        const action = await showOverview(ctx, await collectFamulusInfo(deps), deps.configPath,
          deps.enrollMessagingToken ?? ((token) => exchangeMessagingToken(deps.managerPath, deps.configPath, token)));
        if (action === "reconnect") {
          const outcome = await requestMessagingReconnect(deps.getClient());
          ctx.ui.notify(outcome.text, outcome.error ? "error" : "info");
          continue;
        }
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
