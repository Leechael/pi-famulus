import { accessSync, constants, readFileSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { getSystemErrorMap } from "node:util";

// Filesystem failures mean this optional package is unusable, regardless of the syscall code.
const systemErrorCodes = new Set([...getSystemErrorMap().values()].map(([code]) => code));
const resolutionErrorCodes = new Set([
  "MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND", "ERR_INVALID_PACKAGE_CONFIG",
  "ERR_INVALID_PACKAGE_TARGET", "ERR_PACKAGE_PATH_NOT_EXPORTED",
  "ERR_PACKAGE_IMPORT_NOT_DEFINED", "ERR_UNSUPPORTED_DIR_IMPORT",
  "ERR_INVALID_MODULE_SPECIFIER", "ERR_UNSUPPORTED_RESOLVE_REQUEST",
]);

const packageVersion = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")).version;
const resolveInstalled = createRequire(import.meta.url).resolve;

/** The native optional packages are selected by npm's os/cpu constraints. */
export function nativePackageName(platform, arch) {
  if (!["linux", "darwin", "win32"].includes(platform) || !["x64", "arm64"].includes(arch)) return null;
  // Windows ARM64 has no release producer yet; manual binary lookup still works.
  if (platform === "win32" && arch === "arm64") return null;
  // npm package names use `win` (not `win32`) after unscoped `*-win32-*` names
  // were blocked by registry spam detection on first publish.
  const packageOs = platform === "win32" ? "win" : platform;
  return `pi-famulus-${packageOs}-${arch}`;
}

/** Resolve the exact-version optional package without loading native executable bytes. */
export function resolveNativeManagerPath({ platform = process.platform, arch = process.arch, resolve = resolveInstalled } = {}) {
  const name = nativePackageName(platform, arch);
  if (!name) return null;
  try {
    const metadata = JSON.parse(readFileSync(resolve(`${name}/package.json`), "utf8"));
    if (!metadata || metadata.name !== name || metadata.version !== packageVersion) return null;
    const binary = resolve(`${name}/bin/pi-famulus`);
    const stat = statSync(binary);
    if (!stat.isFile() || stat.size <= 0) return null;
    accessSync(binary, constants.X_OK);
    return binary;
  } catch (error) {
    if (error instanceof SyntaxError || systemErrorCodes.has(error?.code) || resolutionErrorCodes.has(error?.code)) return null;
    throw error;
  }
}
