import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // The test decides the environment, not the developer's machine.
    globalSetup: ["tests/setup/global-env.ts"],
    setupFiles: ["tests/setup/hermetic-env.ts"],
  },
});
