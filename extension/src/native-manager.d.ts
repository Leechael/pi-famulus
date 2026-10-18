export interface NativeManagerOptions {
  platform?: string;
  arch?: string;
  /** Node package resolver; tests can use createRequire from an isolated consumer. */
  resolve?: (specifier: string) => string;
  /** Walk node_modules from this file instead of this module. */
  from?: string;
  /** Do not walk above this directory. */
  stopAt?: string;
  /** Versioned platform cache. `false` disables the cache lookup. */
  cacheDir?: string | false;
}
export interface NativeCacheOptions {
  version?: string;
  platform?: string;
  arch?: string;
  env?: NodeJS.ProcessEnv;
  home?: string;
}
export interface EnsureNativeOptions extends NativeManagerOptions {
  env?: NodeJS.ProcessEnv;
  npmCommand?: string;
  spawn?: (
    command: string,
    args: string[],
    options?: { timeout?: number; windowsHide?: boolean; encoding?: "utf8" },
  ) => { status: number | null; error?: Error };
}
export function nativePackageName(platform: string, arch: string): string | null;
export function nativeCacheDir(options?: NativeCacheOptions): string;
/** Null when unsupported, missing, corrupt, non-executable, or a different version. */
export function resolveNativeManagerPath(options?: NativeManagerOptions): string | null;
/** `npm <package> <path>` or why the exact platform package was not usable. */
export function describeNativeLookup(options?: NativeManagerOptions): string;
/**
 * Install the exact platform package into the versioned cache when it is missing.
 * Returns the binary path, or null when it is still unusable.
 */
export function ensureNativeManagerInstalled(options?: EnsureNativeOptions): string | null;
