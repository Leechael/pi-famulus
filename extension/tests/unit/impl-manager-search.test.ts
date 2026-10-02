import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  DEFAULT_CONFIG,
  MANAGER_FILE_NAME,
  describeManagerSearch,
  getFamulusHome,
  famulusPaths,
  resolveManagerPath,
} from "../../src/config";

// Windows has no execute bit: X_OK only checks existence there.
const itPosix = it.skipIf(process.platform === "win32");

describe("degraded-startup manager search description", () => {
  it("names every place looked and flags configured paths that do not exist", () => {
    const text = describeManagerSearch(
      { managerPath: "/nope/pi-famulus" } as never,
      "/home/u/.pi/agent/pi-famulus",
      { PI_FAMULUS_MANAGER_PATH: "/also/missing" },
    );
    expect(text).toContain("config managerPath /nope/pi-famulus (missing, ignored)");
    expect(text).toContain("PI_FAMULUS_MANAGER_PATH /also/missing (missing, ignored)");
    expect(text).toContain(join("/home/u/.pi/agent/pi-famulus", "bin", MANAGER_FILE_NAME));
    expect(text).toContain("pi-famulus on PATH");
  });
});

describe("Famulus home and binary lookup", () => {
  let root = "";
  afterEach(() => {
    if (root) rmSync(root, { recursive: true, force: true });
    root = "";
  });

  it("uses the new default home and environment override", () => {
    const expected = join(homedir(), ".pi", "agent", "pi-famulus");
    expect(getFamulusHome({})).toBe(expected);
    expect(getFamulusHome({ PI_FAMULUS_HOME: "   " })).toBe(expected);
    expect(getFamulusHome({ PI_FAMULUS_HOME: "/custom/famulus" })).toBe("/custom/famulus");
    expect(famulusPaths(expected, "linux")).toMatchObject({
      socket: join(expected, "manager.sock"),
      pidFile: join(expected, "manager.pid"),
      spawnLock: join(expected, "manager.spawn.lock"),
      log: join(expected, "manager.log"),
    });
  });

  // Same vectors as `lifecycle::tests::windows_pipe_ident_vectors` in the
  // manager: both sides must name the same pipe for one home, however it is
  // spelled, including non-ASCII user names.
  it.each([
    ["C:\\Users\\runneradmin\\.pi\\agent\\pi-famulus", "70d9f71744070b1c"],
    ["C:/Users/RunnerAdmin/.pi/agent/pi-famulus/", "70d9f71744070b1c"],
    ["C:\\Users\\张三\\.pi\\agent\\pi-famulus", "3d482c281b363211"],
    ["D:\\", "cb481618f4f646d5"],
    ["D:/famulus\\\\", "44a0c6fb5c0148ee"],
  ])("names the Windows pipe of %s like the manager does", (home, hash) => {
    expect(famulusPaths(home, "win32").socket).toBe(`\\\\.\\pipe\\pi-famulus-${hash}`);
  });

  it("looks up pi-famulus in config, env, home/bin, then executable PATH order", () => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-search-"));
    const home = join(root, "home");
    const pathDir = join(root, "path");
    const bundled = join(home, "bin", MANAGER_FILE_NAME);
    const onPath = join(pathDir, MANAGER_FILE_NAME);
    const envBinary = join(root, "env-binary");
    const configured = join(root, "configured-binary");
    mkdirSync(join(home, "bin"), { recursive: true });
    mkdirSync(pathDir);
    for (const path of [bundled, onPath, envBinary, configured]) {
      writeFileSync(path, "");
      chmodSync(path, 0o755);
    }
    const env = { PI_FAMULUS_MANAGER_PATH: envBinary, PATH: pathDir };
    expect(resolveManagerPath({ ...DEFAULT_CONFIG, managerPath: configured }, home, env)).toBe(configured);
    expect(resolveManagerPath(DEFAULT_CONFIG, home, env)).toBe(envBinary);
    expect(resolveManagerPath(DEFAULT_CONFIG, home, { ...env, PI_FAMULUS_MANAGER_PATH: "/missing" })).toBe(bundled);
    rmSync(bundled);
    expect(resolveManagerPath(DEFAULT_CONFIG, home, { PATH: pathDir })).toBe(onPath);
    if (process.platform === "win32") return;
    chmodSync(onPath, 0o644);
    expect(resolveManagerPath(DEFAULT_CONFIG, home, { PATH: pathDir })).toBeNull();
  });

  itPosix("skips a stale non-executable home/bin candidate in favor of executable PATH", () => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-search-"));
    const home = join(root, "home");
    const pathDir = join(root, "path");
    const bundled = join(home, "bin", MANAGER_FILE_NAME);
    const onPath = join(pathDir, MANAGER_FILE_NAME);
    mkdirSync(join(home, "bin"), { recursive: true });
    mkdirSync(pathDir);
    writeFileSync(bundled, "");
    chmodSync(bundled, 0o644);
    writeFileSync(onPath, "");
    chmodSync(onPath, 0o755);

    expect(resolveManagerPath(DEFAULT_CONFIG, home, { PATH: pathDir })).toBe(onPath);
    expect(describeManagerSearch(DEFAULT_CONFIG, home, { PATH: pathDir })).toContain(
      `${bundled} (not executable, ignored)`,
    );
    rmSync(onPath);
    expect(resolveManagerPath(DEFAULT_CONFIG, home, { PATH: pathDir })).toBeNull();
  });

  itPosix.each(["config", "env"])("skips a non-executable %s candidate", (source) => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-search-"));
    const candidate = join(root, "stale-binary");
    const onPath = join(root, MANAGER_FILE_NAME);
    writeFileSync(candidate, "");
    chmodSync(candidate, 0o644);
    writeFileSync(onPath, "");
    chmodSync(onPath, 0o755);
    const config = { ...DEFAULT_CONFIG, managerPath: source === "config" ? candidate : null };
    const env = { PATH: root, PI_FAMULUS_MANAGER_PATH: source === "env" ? candidate : undefined };

    expect(resolveManagerPath(config, root, env)).toBe(onPath);
    expect(describeManagerSearch(config, root, env)).toContain(`${candidate} (not executable, ignored)`);
  });

  it.each(["config", "env", "home/bin", "PATH"])("rejects a directory as the %s candidate", (source) => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-search-"));
    const home = join(root, "home");
    const pathDir = join(root, "path");
    const fallbackDir = join(root, "fallback");
    const candidate = source === "home/bin"
      ? join(home, "bin", MANAGER_FILE_NAME)
      : source === "PATH" ? join(pathDir, MANAGER_FILE_NAME) : join(root, "candidate");
    mkdirSync(candidate, { recursive: true });
    chmodSync(candidate, 0o755);
    mkdirSync(fallbackDir);
    const fallback = join(fallbackDir, MANAGER_FILE_NAME);
    writeFileSync(fallback, "");
    chmodSync(fallback, 0o755);
    const config = { ...DEFAULT_CONFIG, managerPath: source === "config" ? candidate : null };
    const env = {
      PATH: [pathDir, fallbackDir].join(delimiter),
      PI_FAMULUS_MANAGER_PATH: source === "env" ? candidate : undefined,
    };

    expect(resolveManagerPath(config, home, env)).toBe(fallback);
    if (source !== "PATH") {
      expect(describeManagerSearch(config, home, env)).toContain(`${candidate} (not a regular file, ignored)`);
    }
    rmSync(fallback);
    expect(resolveManagerPath(config, home, env)).toBeNull();
  });
});
