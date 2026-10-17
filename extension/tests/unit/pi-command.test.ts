import { describe, expect, it } from "vitest";
import { parentResourceArgs } from "../../src/pi-command";

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
