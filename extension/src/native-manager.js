import { spawnSync } from "node:child_process";
import { accessSync, closeSync, constants, mkdirSync, openSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync, writeSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { getSystemErrorMap } from "node:util";

// Filesystem failures mean this optional package is unusable, regardless of the syscall code.
const systemErrorCodes = new Set([...getSystemErrorMap().values()].map(([code]) => code));
const resolutionErrorCodes = new Set([
  "MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND", "ERR_INVALID_PACKAGE_CONFIG",
  "ERR_INVALID_PACKAGE_TARGET", "ERR_PACKAGE_PATH_NOT_EXPORTED",
  "ERR_PACKAGE_IMPORT_NOT_DEFINED", "ERR_UNSUPPORTED_DIR_IMPORT",
  "ERR_INVALID_MODULE_SPECIFIER", "ERR_UNSUPPORTED_RESOLVE_REQUEST",
]);

const modulePath = fileURLToPath(import.meta.url);
const packageVersion = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")).version;
const resolveInstalled = createRequire(import.meta.url).resolve;
const RESTORE_HINT = "not installed; run `npm install --include=optional pi-famulus`";

/** The native optional packages are selected by npm's os/cpu constraints. */
export function nativePackageName(platform, arch) {
  if (!["linux", "darwin", "win32"].includes(platform) || !["x64", "arm64"].includes(arch)) return null;
  // npm package names use `win` (not `win32`) after unscoped `*-win32-*` names
  // were blocked by registry spam detection on first publish.
  const packageOs = platform === "win32" ? "win" : platform;
  return `pi-famulus-${packageOs}-${arch}`;
}

/**
 * Cache for the exact platform package. `pi update --extensions` reinstalls
 * with npm, and `omit=optional` (user or project npmrc) deletes optional
 * dependencies during that install. The cache is outside the famulus runtime
 * home and outside pi's package tree, so the next update does not remove it.
 */
export function nativeCacheDir({
  version = packageVersion,
  platform = process.platform,
  arch = process.arch,
  env = process.env,
  home = homedir(),
} = {}) {
  const base = platform === "win32"
    ? join(env.LOCALAPPDATA || join(home, "AppData", "Local"), "pi-famulus", "native")
    : join(env.XDG_CACHE_HOME || join(home, ".cache"), "pi-famulus", "native");
  return join(base, version, `${platform}-${arch}`);
}

function isIgnoredResolveError(error) {
  return error instanceof SyntaxError || systemErrorCodes.has(error?.code) || resolutionErrorCodes.has(error?.code);
}

function executableFile(path) {
  try {
    const stat = statSync(path);
    if (!stat.isFile() || stat.size <= 0) return false;
    accessSync(path, constants.X_OK);
    return true;
  } catch (error) {
    if (isIgnoredResolveError(error)) return false;
    throw error;
  }
}

/** Usable exact-version binary inside one package directory, or null. */
function binaryInPackage(dir, name) {
  let metadata;
  try {
    metadata = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
  } catch (error) {
    if (isIgnoredResolveError(error)) return null;
    throw error;
  }
  if (!metadata || metadata.name !== name || metadata.version !== packageVersion) return null;
  const exported = metadata.exports?.["./bin/pi-famulus"];
  const rel = typeof exported === "string" ? exported : metadata.main;
  if (typeof rel !== "string" || !rel.startsWith("./") || rel.split("/").includes("..")) return null;
  const binary = join(dir, rel);
  const root = dir.endsWith(sep) ? dir : dir + sep;
  if (!binary.startsWith(root)) return null;
  return executableFile(binary) ? binary : null;
}

function walkNodeModules(startFile, stopAt) {
  const dirs = [];
  let dir = dirname(startFile);
  const seen = new Set();
  const boundary = stopAt ? resolve(stopAt) : null;
  while (!seen.has(dir)) {
    seen.add(dir);
    dirs.push(join(dir, "node_modules"));
    if (boundary && (dir === boundary || !dir.startsWith(boundary.endsWith(sep) ? boundary : boundary + sep))) break;
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return dirs;
}

function legacyResolve(name, resolve) {
  const metadata = JSON.parse(readFileSync(resolve(`${name}/package.json`), "utf8"));
  if (!metadata || metadata.name !== name || metadata.version !== packageVersion) return null;
  const binary = resolve(`${name}/bin/pi-famulus`);
  const stat = statSync(binary);
  if (!stat.isFile() || stat.size <= 0) return null;
  accessSync(binary, constants.X_OK);
  return binary;
}

function searchNative({ platform = process.platform, arch = process.arch, from, stopAt, cacheDir } = {}) {
  const name = nativePackageName(platform, arch);
  if (!name) return { name: null, path: null, detail: `unsupported ${platform}/${arch}` };
  let start = from ?? modulePath;
  if (!from) {
    try { start = realpathSync(modulePath); } catch { /* keep the URL path */ }
  }
  const seen = new Set();
  for (const nodeModules of walkNodeModules(start, stopAt)) {
    const dir = join(nodeModules, name);
    if (seen.has(dir)) continue;
    seen.add(dir);
    const binary = binaryInPackage(dir, name);
    if (binary) return { name, path: binary, detail: "" };
  }
  // NODE_PATH and other resolver roots. An explicit `from` is a hermetic walk.
  if (!from) {
    try {
      const resolved = dirname(resolveInstalled(`${name}/package.json`));
      if (!seen.has(resolved)) {
        const binary = binaryInPackage(resolved, name);
        if (binary) return { name, path: binary, detail: "" };
      }
    } catch (error) {
      if (!isIgnoredResolveError(error)) throw error;
    }
  }
  if (cacheDir !== false) {
    const dir = join(cacheDir ?? nativeCacheDir({ platform, arch }), "node_modules", name);
    const binary = binaryInPackage(dir, name);
    if (binary) return { name, path: binary, detail: "" };
  }
  return { name, path: null, detail: RESTORE_HINT };
}

/** Resolve the exact-version optional package without loading native executable bytes. */
export function resolveNativeManagerPath({ platform = process.platform, arch = process.arch, resolve, from, stopAt, cacheDir } = {}) {
  const name = nativePackageName(platform, arch);
  if (!name) return null;
  // Injected resolvers stay hermetic: tests use them to simulate one install tree.
  if (resolve) {
    try {
      return legacyResolve(name, resolve);
    } catch (error) {
      if (isIgnoredResolveError(error)) return null;
      throw error;
    }
  }
  return searchNative({ platform, arch, from, stopAt, cacheDir }).path;
}

/** `npm <package> <path>` or why that exact platform package was not usable. */
export function describeNativeLookup(options) {
  const name = nativePackageName(options?.platform ?? process.platform, options?.arch ?? process.arch);
  if (!name) return `npm native manager (unsupported ${options?.platform ?? process.platform}/${options?.arch ?? process.arch})`;
  if (options?.resolve) {
    const path = resolveNativeManagerPath(options);
    return path ? `npm ${name} ${path}` : `npm ${name} (missing or unusable, ignored)`;
  }
  const found = searchNative(options);
  return found.path ? `npm ${name} ${found.path}` : `npm ${name} (${found.detail})`;
}

function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/** A lock left by a dead process, or held far longer than one npm install, can be replaced. */
function installLockIsStale(lockPath) {
  try {
    const age = Date.now() - statSync(lockPath).mtimeMs;
    const pid = Number(readFileSync(lockPath, "utf8").trim());
    if (Number.isInteger(pid) && pid > 0) return !pidAlive(pid) || age > 150_000;
    return age > 5_000;
  } catch {
    return true;
  }
}

/**
 * One npm install per cache directory. Another startup either waits until that
 * install publishes a binary or takes over a stale lock. Returns the open lock,
 * or null when waiting itself produced a usable binary.
 */
function acquireInstallLock(dir, lookup) {
  mkdirSync(dir, { recursive: true });
  const lockPath = `${dir}.install.lock`;
  const deadline = Date.now() + 130_000;
  while (Date.now() <= deadline) {
    try {
      const fd = openSync(lockPath, "wx");
      try { writeSync(fd, String(process.pid)); } catch { /* the fd is still the lock */ }
      return { fd, lockPath };
    } catch (error) {
      if (error?.code !== "EEXIST") throw error;
      const ready = resolveNativeManagerPath(lookup);
      if (ready) return null;
      if (installLockIsStale(lockPath)) {
        rmSync(lockPath, { force: true });
        continue;
      }
      sleepMs(200);
    }
  }
  return null;
}

function releaseInstallLock(lock) {
  if (!lock) return;
  try { closeSync(lock.fd); } catch { /* already closed */ }
  rmSync(lock.lockPath, { force: true });
}

/**
 * Install the exact platform package into the versioned cache when the
 * optional dependency is missing. No-ops when a usable binary is already
 * resolvable. `PI_FAMULUS_NO_NATIVE_FETCH=1`, `PI_OFFLINE=1`, and Vitest skip
 * the install. Directory, manifest, and npm failures return null so startup
 * degrades instead of throwing.
 */
export function ensureNativeManagerInstalled({
  platform = process.platform,
  arch = process.arch,
  cacheDir,
  env = process.env,
  npmCommand,
  spawn = spawnSync,
  from,
  stopAt,
} = {}) {
  const lookup = { platform, arch, cacheDir, from, stopAt };
  try {
    const existing = resolveNativeManagerPath(lookup);
    if (existing) return existing;
    if (!nativePackageName(platform, arch)) return null;
    if (env.PI_FAMULUS_NO_NATIVE_FETCH === "1" || env.PI_OFFLINE === "1" || env.VITEST) return null;
    if (cacheDir === false) return null;
    const dir = cacheDir ?? nativeCacheDir({ platform, arch, env });
    const lockedLookup = { ...lookup, cacheDir: dir };
    const lock = acquireInstallLock(dir, lockedLookup);
    if (!lock) return resolveNativeManagerPath(lockedLookup);
    try {
      const manifest = join(dir, "package.json");
      try {
        readFileSync(manifest);
      } catch {
        writeFileSync(manifest, `${JSON.stringify({ private: true, name: "pi-famulus-native-cache", version: "0.0.0" })}\n`);
      }
      const command = npmCommand ?? (platform === "win32" ? "npm.cmd" : "npm");
      const name = nativePackageName(platform, arch);
      let result;
      try {
        result = spawn(command, [
          "install", `${name}@${packageVersion}`,
          "--prefix", dir,
          "--ignore-scripts", "--no-fund", "--no-audit", "--loglevel=error",
        ], { timeout: 120_000, windowsHide: true, encoding: "utf8" });
      } catch {
        return resolveNativeManagerPath(lockedLookup);
      }
      const found = resolveNativeManagerPath(lockedLookup);
      if (found) return found;
      if (!result || result.error || result.status !== 0) return null;
      return null;
    } finally {
      releaseInstallLock(lock);
    }
  } catch {
    return null;
  }
}
