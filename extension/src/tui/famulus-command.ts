/**
 * /famulus — which pi-famulus binary this session uses and which versions run.
 */
import { execFile } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { ManagerClient } from "../manager-client";

export interface FamulusCommandDeps {
  extensionVersion: string;
  managerPath: string | null;
  /** Where the binary was looked for, shown when none was found. */
  describeSearch: () => string;
  getClient: () => ManagerClient | null;
  /** `<binary> --version`; null when it cannot be run. */
  binaryVersion?: (path: string) => Promise<string | null>;
}

export interface FamulusInfo {
  extensionVersion: string;
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
  ].join("\n");
}

export function readBinaryVersion(path: string): Promise<string | null> {
  return new Promise((resolve) => {
    execFile(path, ["--version"], { timeout: 2000 }, (err, stdout) => {
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
    managerPath: deps.managerPath,
    searched: deps.managerPath ? "" : deps.describeSearch(),
    binaryVersion: deps.managerPath ? await (deps.binaryVersion ?? readBinaryVersion)(deps.managerPath) : null,
    daemon: connected ? client?.managerInfo() ?? null : null,
    daemonError: connected ? "" : client?.lastError() ?? "",
  };
}

export function registerFamulusCommand(pi: ExtensionAPI, deps: FamulusCommandDeps): void {
  pi.registerCommand("famulus", {
    description: "Show the pi-famulus binary path and the manager and extension versions",
    handler: async (_args, ctx) => {
      ctx.ui.notify(formatFamulusInfo(await collectFamulusInfo(deps)), "info");
    },
  });
}
