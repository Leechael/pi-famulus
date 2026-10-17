/**
 * Find the pi command a hosted subagent should be. The daemon never searches
 * PATH; the extension resolves it where pi is already running.
 */
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
export type PiCommand = { argv: string[] } | { error: string };

export function resolvePiArgv(configured?: string | null): PiCommand {
  const raw = (configured && configured.trim()) || process.env.PI_FAMULUS_PI?.trim() || "";
  if (raw) {
    if (raw.startsWith("[")) {
      try {
        const argv = JSON.parse(raw) as unknown;
        if (Array.isArray(argv) && argv.every((part) => typeof part === "string" && part.length > 0)) {
          return { argv: argv as string[] };
        }
      } catch {
        return { error: `PI_FAMULUS_PI / subagent.piCommand is not a JSON argv array: ${raw}` };
      }
      return { error: `PI_FAMULUS_PI / subagent.piCommand is not a JSON argv array: ${raw}` };
    }
    return { argv: raw.includes(" ") ? splitCommand(raw) : [raw] };
  }
  const exec = process.execPath;
  const entry = process.argv[1] ?? "";
  if (entry.includes("pi-coding-agent") && existsSync(entry)) return { argv: [exec, entry] };
  try {
    const req = createRequire(import.meta.url);
    const pkg = req.resolve("@earendil-works/pi-coding-agent/package.json");
    const cli = pkg.replace(/package\.json$/, "dist/cli.js");
    if (existsSync(cli)) return { argv: [exec, cli] };
  } catch {
    // The host did not load pi as a resolvable package.
  }
  return {
    error:
      "cannot find the pi executable that is running this session. Set subagent.piCommand in config.json or PI_FAMULUS_PI to the pi command.",
  };
}

const PARENT_FLAGS = new Set([
  "-ne",
  "--no-extensions",
  "-ns",
  "--no-skills",
  "-np",
  "--no-prompt-templates",
  "-nc",
  "--no-context-files",
  "--offline",
  "--no-mcp",
]);

/**
 * Flags from the pi process that is spawning the subagent. The child is a new
 * process, so it only sees providers and tools that its own command line loads.
 * Eval and SDK hosts pass those as `-e` on argv (`-ne` plus an explicit faux
 * provider extension); forwarding them is what lets the child use the same model.
 */
export function parentResourceArgs(argv: readonly string[] = process.argv.slice(2)): string[] {
  const out: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "-e" || arg === "--extension") {
      const value = argv[i + 1];
      if (value && !value.startsWith("-")) {
        out.push("-e", value);
        i++;
      }
      continue;
    }
    if (PARENT_FLAGS.has(arg)) out.push(arg);
  }
  return out;
}

function splitCommand(raw: string): string[] {
  const parts: string[] = [];
  let current = "";
  let quote: '"' | "'" | null = null;
  for (const ch of raw) {
    if (quote) {
      if (ch === quote) quote = null;
      else current += ch;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      continue;
    }
    if (ch === " ") {
      if (current) parts.push(current);
      current = "";
      continue;
    }
    current += ch;
  }
  if (current) parts.push(current);
  return parts;
}
