/**
 * Find the pi command a hosted subagent should be. The daemon never searches
 * PATH; the extension resolves it where pi is already running.
 */
import { existsSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { basename } from "node:path";
export type PiCommand = { argv: string[] } | { error: string };

export function resolvePiArgv(configured?: string | null, argv: readonly string[] = process.argv): PiCommand {
  const raw = (configured && configured.trim()) || process.env.PI_FAMULUS_PI?.trim() || "";
  if (raw) {
    if (raw.startsWith("[")) {
      try {
        const parsed = JSON.parse(raw) as unknown;
        if (Array.isArray(parsed) && parsed.length > 0 && parsed.every((part) => typeof part === "string" && part.length > 0)) {
          return { argv: parsed as string[] };
        }
      } catch {
        return { error: `PI_FAMULUS_PI / subagent.piCommand is not a JSON argv array: ${raw}` };
      }
      return { error: `PI_FAMULUS_PI / subagent.piCommand is not a JSON argv array: ${raw}` };
    }
    return { argv: raw.includes(" ") ? splitCommand(raw) : [raw] };
  }
  const fromProcess = runningPiArgv(argv);
  if (fromProcess) return { argv: fromProcess };
  const exec = process.execPath;
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

/**
 * The pi process that loaded this extension. `node_modules/.bin/pi` is a
 * symlink (eval and `npm exec`); its path does not contain `pi-coding-agent`,
 * so matching only that substring misses the process that is actually running.
 */
function runningPiArgv(argv: readonly string[]): string[] | null {
  const entry = argv[1] ?? "";
  if (!entry || !existsSync(entry)) return null;
  let real = entry;
  try {
    real = realpathSync(entry);
  } catch {
    // A broken symlink still has a path; keep the original.
  }
  const base = basename(entry).toLowerCase();
  const namedPi = base === "pi" || base === "pi.cmd" || base === "pi.exe" || entry.includes("pi-coding-agent") || real.includes("pi-coding-agent");
  if (!namedPi) return null;
  if (/\.[cm]?js$/i.test(real)) return [process.execPath, real];
  return [entry];
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
