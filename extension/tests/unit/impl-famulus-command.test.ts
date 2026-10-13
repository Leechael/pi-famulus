import { describe, expect, it } from "vitest";
import { collectFamulusInfo, formatFamulusInfo } from "../../src/tui/famulus-command";
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
      ].join("\n"),
    );
  });

  it("says where it looked when no binary is found and why the manager is not connected", async () => {
    const info = await collectFamulusInfo({
      extensionVersion: "0.1.6",
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
  });
});
