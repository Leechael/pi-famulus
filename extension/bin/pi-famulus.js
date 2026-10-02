#!/usr/bin/env node
import { spawn } from "node:child_process";
import { nativePackageName, resolveNativeManagerPath } from "../src/native-manager.js";

const binary = resolveNativeManagerPath();
if (!binary) {
  const name = nativePackageName(process.platform, process.arch);
  console.error(name
    ? `pi-famulus: missing usable ${name}. Reinstall pi-famulus with optional dependencies enabled (npm install --include=optional pi-famulus).`
    : `pi-famulus: unsupported ${process.platform}/${process.arch}; only Linux/macOS/Windows x64/arm64 are supported.`);
  process.exit(1);
}
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
// Supervisors often signal the wrapper PID, not the whole process group.
const forwarders = new Map(["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"].map(signal => {
  const forward = () => child.kill(signal);
  process.on(signal, forward);
  return [signal, forward];
}));
let failed = false;
child.once("error", error => {
  failed = true;
  console.error(`pi-famulus: cannot execute native manager: ${error.message}`);
});
child.once("close", (code, signal) => {
  for (const [name, forward] of forwarders) process.off(name, forward);
  process.exitCode = failed ? 1 : (code ?? 1);
  if (signal && !failed) process.kill(process.pid, signal);
});
