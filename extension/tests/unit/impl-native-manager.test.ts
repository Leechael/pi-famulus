import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { nativePackageName, resolveNativeManagerPath } from "../../src/native-manager.js";
import pkg from "../../package.json";

describe("npm native manager selection", () => {
  it.each([
    ["linux", "x64", "pi-famulus-linux-x64"],
    ["linux", "arm64", "pi-famulus-linux-arm64"],
    ["darwin", "x64", "pi-famulus-darwin-x64"],
    ["darwin", "arm64", "pi-famulus-darwin-arm64"],
    ["win32", "x64", "pi-famulus-win32-x64"],
    ["win32", "arm64", "pi-famulus-win32-arm64"],
  ])("selects %s/%s", (platform, arch, name) => {
    expect(nativePackageName(platform, arch)).toBe(name);
  });

  it.each([["linux", "ia32"], ["freebsd", "arm64"]])(
    "does not invent an unsupported %s/%s package", (platform, arch) => {
      expect(nativePackageName(platform, arch)).toBeNull();
      expect(resolveNativeManagerPath({ platform, arch })).toBeNull();
    },
  );
});

describe("installed native manager", () => {
  let root = "";
  afterEach(() => {
    if (root) rmSync(root, { recursive: true, force: true });
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
