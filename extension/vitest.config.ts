import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The test decides the environment, not the developer's machine.
    globalSetup: ["tests/setup/global-env.ts"],
    setupFiles: ["tests/setup/hermetic-env.ts"],
    // Windows runners spawn processes slowly enough that tests which pass in
    // a second elsewhere hit vitest's 5s default there. Tests that need a
    // specific bound still set their own.
    testTimeout: process.platform === "win32" ? 30_000 : 5_000,
  },
});
