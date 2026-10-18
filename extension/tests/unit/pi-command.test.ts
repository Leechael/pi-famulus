import { mkdirSync, mkdtempSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { parentResourceArgs, resolvePiArgv } from "../../src/pi-command";

describe("parentResourceArgs", () => {
  it("forwards the extensions and isolation flags the parent pi was started with", () => {
    expect(parentResourceArgs([
      "--mode", "rpc",
      "-ne", "-ns", "-np", "-nc",
      "--no-session",
      "--offline",
      "-e", "/opt/pi-famulus",
      "-e", "/opt/faux-ext.ts",
      "--model", "faux/faux-1",
    ])).toEqual([
      "-ne", "-ns", "-np", "-nc",
      "--offline",
      "-e", "/opt/pi-famulus",
      "-e", "/opt/faux-ext.ts",
    ]);
  });
});

describe("resolvePiArgv", () => {
  it("uses the pi launcher that is running this process, including a .bin symlink", () => {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "pi-argv-")));
    const cli = join(dir, "pi-coding-agent", "dist", "cli.js");
    mkdirSync(join(dir, "pi-coding-agent", "dist"), { recursive: true });
    writeFileSync(cli, "// pi\n");
    const bin = join(dir, "node_modules", ".bin", "pi");
    mkdirSync(join(dir, "node_modules", ".bin"), { recursive: true });
    symlinkSync(cli, bin);
    const prev = process.env.PI_FAMULUS_PI;
    delete process.env.PI_FAMULUS_PI;
    try {
      expect(resolvePiArgv(null, ["node", bin, "--mode", "rpc"])).toEqual({
        argv: [process.execPath, cli],
      });
      expect(resolvePiArgv(null, ["node", cli])).toEqual({ argv: [process.execPath, cli] });
    } finally {
      if (prev !== undefined) process.env.PI_FAMULUS_PI = prev;
    }
  });
});
