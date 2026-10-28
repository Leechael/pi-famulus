/**
 * The test environment comes from tests/setup/hermetic-env.ts, not the host.
 * Each check names a route by which the developer's machine used to reach a
 * test.
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir, tmpdir, userInfo } from "node:os";
import { join, relative, isAbsolute } from "node:path";
import { getAgentDir } from "@earendil-works/pi-coding-agent";
import { describe, expect, it } from "vitest";
import { getFamulusHome } from "../../src/config";
import { nativeCacheDir } from "../../src/native-manager.js";

const realHome = userInfo().homedir;
const sandboxHome = process.env.HOME ?? "";
const inside = (dir: string, path: string) => {
  const rel = relative(dir, path);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
};

describe("hermetic test environment", () => {
  it("puts every per-user location in a temp sandbox, away from the real home", () => {
    expect(sandboxHome).not.toBe("");
    expect(inside(tmpdir(), sandboxHome)).toBe(true);
    expect(homedir()).toBe(sandboxHome);
    expect(homedir()).not.toBe(realHome);
    for (const path of [getFamulusHome(), nativeCacheDir(), getAgentDir()]) {
      expect(inside(sandboxHome, path), path).toBe(true);
    }
    expect(existsSync(join(getFamulusHome(), "manager.sock"))).toBe(false);
  });

  it("drops inherited pi, proxy, colour and terminal settings", () => {
    const leaked = Object.keys(process.env).filter((key) =>
      /^(PI_|HERDR_)/i.test(key) && !["PI_FAMULUS_INTEG", "PI_FAMULUS_TEST_ARTIFACTS", "PI_FAMULUS_MANAGER_PATH"].includes(key)
      || /^(https?_proxy|all_proxy|no_proxy|no_color|force_color|columns|lines|tmux)$/i.test(key));
    expect(leaked).toEqual([]);
    if (process.env.PI_FAMULUS_INTEG !== "1") expect(process.env.PI_FAMULUS_MANAGER_PATH).toBeUndefined();
    expect(process.env.TZ).toBe("UTC");
    expect(new Date(0).getHours()).toBe(0);
  });

  it("children inherit the sandbox and a PATH without the host's tools", () => {
    const tools = ["pi-famulus", "pi", "claude", "codex", "herdr"];
    const probe = `
      const { existsSync } = require("node:fs");
      const { join, delimiter } = require("node:path");
      const names = ${JSON.stringify(tools)}.flatMap((t) => process.platform === "win32" ? [t + ".exe", t + ".cmd", t] : [t]);
      const found = (process.env.PATH ?? process.env.Path ?? "").split(delimiter).filter(Boolean)
        .flatMap((dir) => names.filter((name) => existsSync(join(dir, name))).map((name) => join(dir, name)));
      console.log(JSON.stringify({ home: require("node:os").homedir(), proxy: process.env.HTTP_PROXY ?? null, found }));
    `;
    const result = spawnSync(process.execPath, ["-e", probe], { encoding: "utf8" });
    expect(result.status, result.stderr).toBe(0);
    expect(JSON.parse(result.stdout)).toEqual({ home: sandboxHome, proxy: null, found: [] });
  });
});
