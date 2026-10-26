/**
 * /famulus — which pi-famulus binary this session uses and which versions run.
 */
import { execFile } from "node:child_process";
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../manager-client";

export interface FamulusCommandDeps {
  extensionVersion: string;
  configPath: string;
  managerPath: string | null;
  /** Where the binary was looked for, shown when none was found. */
  describeSearch: () => string;
  getClient: () => ManagerClient | null;
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
  };
}

function readConfigForEdit(path: string): string {
  try {
    return readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return "{}\n";
    throw error;
  }
}

function writeConfig(path: string, text: string): void {
  const parsed: unknown = JSON.parse(text);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error("config.json must contain a JSON object");
  }
  mkdirSync(dirname(path), { recursive: true });
  const temporary = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, `${JSON.stringify(parsed, null, 2)}\n`, "utf8");
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
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
        const action = await ctx.ui.select(formatFamulusInfo(await collectFamulusInfo(deps)), [
          "Edit configuration",
          "Reload extension",
          "Close",
        ]);
        if (action === "Reload extension") {
          await ctx.reload();
          return;
        }
        if (action !== "Edit configuration") return;
        let current: string;
        try {
          current = readConfigForEdit(deps.configPath);
        } catch (error) {
          ctx.ui.notify(`Cannot read ${deps.configPath}: ${(error as Error).message}`, "error");
          continue;
        }
        const edited = await ctx.ui.editor(`Edit ${deps.configPath}`, current);
        if (edited === undefined) continue;
        try {
          writeConfig(deps.configPath, edited);
          ctx.ui.notify(`Saved ${deps.configPath}. Reload the extension to apply extension settings.`, "info");
        } catch (error) {
          ctx.ui.notify(`Configuration was not saved: ${(error as Error).message}`, "error");
        }
      }
    },
  });
}
