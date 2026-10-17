import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  describeNativeLookup,
  ensureNativeManagerInstalled,
  nativePackageName,
  resolveNativeManagerPath,
} from "../../src/native-manager.js";
import pkg from "../../package.json";

describe("npm native manager selection", () => {
  it.each([
    ["linux", "x64", "pi-famulus-linux-x64"],
    ["linux", "arm64", "pi-famulus-linux-arm64"],
    ["darwin", "x64", "pi-famulus-darwin-x64"],
    ["darwin", "arm64", "pi-famulus-darwin-arm64"],
    ["win32", "x64", "pi-famulus-win-x64"],
    ["win32", "arm64", "pi-famulus-win-arm64"],
  ])("selects %s/%s", (platform, arch, name) => {
    expect(nativePackageName(platform, arch)).toBe(name);
  });

  it.each([["linux", "ia32"], ["freebsd", "arm64"], ["win32", "ia32"]])(
    "does not invent an unsupported %s/%s package", (platform, arch) => {
      expect(nativePackageName(platform, arch)).toBeNull();
      expect(resolveNativeManagerPath({ platform, arch })).toBeNull();
    },
  );
});

describe("installed native manager", () => {
  let root = "";
  afterEach(() => {
    if (root) rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    root = "";
  });

  function installed(version = pkg.version) {
    root = realpathSync(mkdtempSync(join(tmpdir(), "native-test-")));
    const native = join(root, "node_modules", "pi-famulus-darwin-arm64");
    mkdirSync(join(native, "bin"), { recursive: true });
    writeFileSync(join(native, "package.json"), JSON.stringify({
      name: "pi-famulus-darwin-arm64", version,
      exports: { "./package.json": "./package.json", "./bin/pi-famulus": "./bin/pi-famulus" },
    }));
    const binary = join(native, "bin", "pi-famulus");
    writeFileSync(binary, "#!/bin/sh\nexit 0\n");
    chmodSync(binary, 0o755);
    return { binary, resolve: createRequire(join(root, "consumer.cjs")).resolve };
  }

  it("discovers the executable installed by the matching optional package", () => {
    const { binary, resolve } = installed();
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBe(binary);
  });

  // Windows has no execute bit to clear.
  it.skipIf(process.platform === "win32")("rejects a non-executable native file rather than shadowing a working manual binary", () => {
    const { binary, resolve } = installed();
    chmodSync(binary, 0o644);
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it("rejects an empty executable rather than shadowing a working manual binary", () => {
    const { binary, resolve } = installed();
    writeFileSync(binary, "");
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it.each(["{", "null"])("ignores malformed native package metadata %s", (text) => {
    const { resolve } = installed();
    writeFileSync(join(root, "node_modules", "pi-famulus-darwin-arm64", "package.json"), text);
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it("ignores a corrupted package with an invalid executable export target", () => {
    const { resolve } = installed();
    writeFileSync(join(root, "node_modules", "pi-famulus-darwin-arm64", "package.json"), JSON.stringify({
      name: "pi-famulus-darwin-arm64", version: pkg.version,
      exports: { "./package.json": "./package.json", "./bin/pi-famulus": "../outside-package" },
    }));
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it.each([
    "EISDIR", "ELOOP", "ENAMETOOLONG", "EIO", "EMFILE", "EBUSY",
    "ERR_INVALID_PACKAGE_TARGET", "ERR_MODULE_NOT_FOUND",
    "ERR_PACKAGE_IMPORT_NOT_DEFINED", "ERR_UNSUPPORTED_DIR_IMPORT",
    "ERR_INVALID_MODULE_SPECIFIER", "ERR_UNSUPPORTED_RESOLVE_REQUEST",
  ].flatMap((code) => ["package.json", "bin/pi-famulus"].map((target) => [code, target])))(
    "permits fallback for %s resolving %s", (code, target) => {
      const { resolve: installedResolve } = installed();
      const resolve = (specifier: string) => {
        if (specifier === `pi-famulus-darwin-arm64/${target}`) throw Object.assign(new Error(code), { code });
        return installedResolve(specifier);
      };
      expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
    },
  );

  it.each([
    new TypeError("resolver programming error"),
    new ReferenceError("resolver programming error"),
    Object.assign(new Error("invalid resolver argument"), { code: "ERR_INVALID_ARG_TYPE" }),
    Object.assign(new Error("unknown failure"), { code: "EAPPLICATION" }),
    new Error("unexpected resolver failure"),
  ])("preserves unexpected resolver errors: %s", (error) => {
    const resolve = () => { throw error; };
    expect(() => resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toThrow(error);
  });

  it("never pairs the extension with a different native package version", () => {
    const { resolve } = installed("999.0.0");
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it.each(["missing", "directory"])("ignores a %s native executable", (kind) => {
    const { binary, resolve } = installed();
    rmSync(binary);
    if (kind === "directory") mkdirSync(binary);
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });

  it("returns null when optional dependencies were omitted, permitting explicit/manual installs", () => {
    root = realpathSync(mkdtempSync(join(tmpdir(), "native-test-")));
    const resolve = createRequire(join(root, "consumer.cjs")).resolve;
    expect(resolveNativeManagerPath({ platform: "darwin", arch: "arm64", resolve })).toBeNull();
  });
});

const hostNativeName = nativePackageName(process.platform, process.arch);
const hostExe = process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus";

describe.skipIf(!hostNativeName)("native package walk and cache", () => {
  let root = "";
  afterEach(() => {
    if (root) rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    root = "";
  });

  const name = hostNativeName!;
  const exe = hostExe;

  function layout() {
    root = realpathSync(mkdtempSync(join(tmpdir(), "native-walk-")));
    const from = join(root, "nested", "src", "native-manager.js");
    mkdirSync(dirname(from), { recursive: true });
    writeFileSync(from, "");
    return from;
  }

  function plant(parent: string, version: string, opts: { binary?: boolean; mode?: number } = {}) {
    if (!name) throw new Error("unsupported platform");
    const dir = join(parent, "node_modules", name);
    mkdirSync(join(dir, "bin"), { recursive: true });
    const rel = `./bin/${exe}`;
    writeFileSync(join(dir, "package.json"), JSON.stringify({
      name, version,
      exports: { "./package.json": "./package.json", "./bin/pi-famulus": rel },
    }));
    const binary = join(dir, "bin", exe);
    if (opts.binary !== false) {
      writeFileSync(binary, "#!/bin/sh\nexit 0\n");
      chmodSync(binary, opts.mode ?? 0o755);
    }
    return binary;
  }

  it("skips a closer unusable package and uses an exact executable higher up", () => {
    const from = layout();
    plant(join(root, "nested"), pkg.version, { binary: false });
    const binary = plant(root, pkg.version);
    const lookup = { from, stopAt: root, cacheDir: false as const };
    expect(resolveNativeManagerPath(lookup)).toBe(binary);
    expect(describeNativeLookup(lookup)).toBe(`npm ${name} ${binary}`);
  });

  it("does not use a different native package version from the cache or the walk", () => {
    const from = layout();
    plant(root, "999.0.0");
    const cache = join(root, "cache");
    plant(cache, "999.0.0");
    const lookup = { from, stopAt: root, cacheDir: cache };
    expect(resolveNativeManagerPath(lookup)).toBeNull();
    expect(describeNativeLookup(lookup)).toContain("npm install --include=optional pi-famulus");
  });

  it("installs the exact platform package into the cache when none is installed", () => {
    const from = layout();
    const cache = join(root, "cache");
    const calls: string[][] = [];
    const found = ensureNativeManagerInstalled({
      from, stopAt: root, cacheDir: cache, env: {},
      spawn(_command, args) {
        calls.push(args);
        plant(cache, pkg.version);
        return { status: 0 };
      },
    });
    expect(calls).toHaveLength(1);
    expect(calls[0]).toContain(`${name}@${pkg.version}`);
    expect(calls[0]?.[calls[0].indexOf("--prefix") + 1]).toBe(cache);
    expect(found).toBe(join(cache, "node_modules", name, "bin", exe));
    expect(ensureNativeManagerInstalled({
      from, stopAt: root, cacheDir: cache, env: {},
      spawn() { throw new Error("already installed"); },
    })).toBe(found);
  });

  it("does not fetch when the optional package was intentionally left missing", () => {
    const from = layout();
    expect(ensureNativeManagerInstalled({
      from, stopAt: root, cacheDir: false, env: { PI_FAMULUS_NO_NATIVE_FETCH: "1" },
      spawn() { throw new Error("fetched"); },
    })).toBeNull();
  });
});
