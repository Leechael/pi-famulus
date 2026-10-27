/**
 * The settings pages of /famulus: every documented config.json key, grouped
 * by who reads it, edited one value at a time (docs/config.md).
 */
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { hostname } from "node:os";
import { dirname } from "node:path";
import type { MessagingStatus } from "../manager-client";
import type { PiTuiInteractive, PiTuiModule, PiTuiSettingItem, PiTuiSettingsListTheme } from "./pi-tui-load";

type Kind = "int" | "text" | "secret" | "bool" | "enum" | "duration";

export interface SettingSpec {
  /** Key path inside config.json. */
  path: string[];
  kind: Kind;
  /** What an unset key means, as shown in the list. */
  default: string;
  description: string;
  values?: string[];
}

export interface SettingsSection {
  id: string;
  label: string;
  /** When a change takes effect, unless a key says otherwise. */
  applies: string;
  specs: Array<SettingSpec & { applies?: string }>;
}

const RELOAD = "reload the extension to apply";
const LIVE = "applies immediately";
const RESTART = "restart the manager daemon to apply";

const int = (path: string[], def: string, description: string, applies?: string) => ({
  path,
  kind: "int" as const,
  default: def,
  description,
  ...(applies ? { applies } : {}),
});

export const SETTINGS_SECTIONS: SettingsSection[] = [
  {
    id: "extension",
    label: "Extension",
    applies: RELOAD,
    specs: [
      int(["foregroundBudgetMs"], "20000", "How long bash waits before moving a command to the background (ms)."),
      int(["subagentBudgetMs"], "45000", "Default wait before a subagent call runs in the background (ms); subagent.budgetMs overrides it."),
      { path: ["managerPath"], kind: "text", default: "auto", description: "Explicit pi-famulus executable; empty resolves it automatically." },
      { path: ["logLevel"], kind: "enum", values: ["debug", "info", "warn", "error"], default: "info", description: "Log level." },
    ],
  },
  {
    id: "subagent",
    label: "Subagents",
    applies: RELOAD,
    specs: [
      int(["subagent", "budgetMs"], "45000", "Per-call backgrounding budget (ms)."),
      int(["subagent", "timeoutMs"], "1800000", "Soft per-turn budget (ms); reaching it does not stop the child."),
      int(["subagent", "overrunRepeatMs"], "600000", "Interval between repeated overrun notifications (ms)."),
      int(["subagent", "hardTimeoutMs"], "0", "Per-turn abort limit (ms); 0 disables it."),
      int(["subagent", "stallMs"], "300000", "Abort a child with no events for this long (ms)."),
      int(["subagent", "stallRetries"], "1", "Automatic retries after a stall."),
      int(["subagent", "stallRetryDelayMs"], "5000", "Delay before retrying a stalled child (ms)."),
      int(["subagent", "decisionTimeoutMs"], "600000", "Maximum wait for the parent to answer a decision request (ms)."),
      int(["subagent", "concurrency"], "4", "Default concurrency within one subagent call."),
      int(["subagent", "maxConcurrentChildren"], "8", "Concurrent children allowed in this pi session."),
      int(["subagent", "spawnBudgetPerHour"], "32", "Child sessions started per hour in this session."),
      { path: ["subagent", "piCommand"], kind: "text", default: "pi", description: "Pi executable, shell command, or JSON argv array for hosted children." },
      int(["subagent", "idleMinMs"], "120000", "Minimum warm idle lifetime before parking (ms). Read by the daemon.", LIVE),
      int(["subagent", "idleGraceMs"], "15000", "Grace period after the estimated prompt-cache expiry (ms). Read by the daemon.", LIVE),
      int(["subagent", "idleMaxMs"], "3600000", "Maximum warm idle lifetime (ms). Read by the daemon.", LIVE),
    ],
  },
  {
    id: "daemon",
    label: "Manager daemon",
    applies: LIVE,
    specs: [
      int(["maxAgents"], "8", "Machine-wide concurrent subagent limit."),
      int(["maxTestSuite"], "2", "Concurrent test-suite subagents."),
      int(["maxTest"], "2", "Concurrent test subagents."),
      int(["maxBuild"], "maxAgents", "Concurrent build subagents."),
      int(["maxLintType"], "maxAgents", "Concurrent lint/type subagents."),
      int(["maxOther"], "maxAgents", "Concurrent general subagents."),
      int(["maxGit"], "maxAgents", "Concurrent git subagents."),
      int(["maxReadSearch"], "maxAgents", "Concurrent read/search subagents."),
      int(["maxWarmAgents"], "4", "Maximum warm hosted agents."),
      { path: ["goneSessionRetention"], kind: "duration", default: "24h", description: "How long files of disconnected sessions are kept (30m, 24h, 7d, 0s).", applies: RESTART },
      { path: ["finishedTaskRetention"], kind: "duration", default: "24h", description: "How long finished task records and output are kept.", applies: RESTART },
    ],
  },
  {
    id: "messaging",
    label: "Messaging",
    applies: RESTART,
    specs: [
      { path: ["messaging", "enabled"], kind: "bool", default: "false", description: "Opt in to cross-session messaging tools, commands, and connections.", applies: "reload the extension and restart the manager daemon to apply" },
      { path: ["messaging", "name"], kind: "text", default: hostname(), description: "Device name shown to peers; the hostname when unset." },
      { path: ["messaging", "server"], kind: "text", default: "https://127.0.0.1:8443", description: "Relay server URL." },
      { path: ["messaging", "token"], kind: "secret", default: "not set", description: "Enrollment token; Enter exchanges it for a saved device credential. The token is not stored.", applies: "exchanges immediately" },
      { path: ["messaging", "localOnly"], kind: "bool", default: "false", description: "Only local sessions, no relay. Enable explicitly for local-only messaging." },
      { path: ["messaging", "remoteMessages"], kind: "enum", values: ["allow", "refuse"], default: "allow", description: "Whether messages from other machines are accepted." },
      { path: ["messaging", "tlsInsecure"], kind: "bool", default: "false", description: "Skip relay TLS certificate verification." },
      { path: ["messaging", "mcpListen"], kind: "text", default: "off", description: "MCP listener address; empty disables it." },
      { path: ["messaging", "mcpToken"], kind: "secret", default: "generated", description: "MCP bearer token; generated and stored privately when unset." },
      { path: ["messaging", "mcpAllowNonLoopback"], kind: "bool", default: "false", description: "Permit a non-loopback MCP listener (needs mcpToken)." },
      { path: ["messaging", "sock"], kind: "text", default: "default", description: "Local bridge socket path." },
      { path: ["messaging", "stateDir"], kind: "text", default: "~/.pi/agent/pi-famulus/messaging", description: "Device identity and bridge state directory." },
      int(["messaging", "eventLogSize"], "4096", "Maximum retained bridge event-log entries."),
    ],
  },
];

type Json = Record<string, unknown>;

export function readConfigText(path: string): string {
  try {
    return readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return "{}\n";
    throw error;
  }
}

function parseConfig(text: string): Json {
  const parsed: unknown = JSON.parse(text);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error("config.json must contain a JSON object");
  }
  return parsed as Json;
}

export function readConfig(path: string): Json {
  return parseConfig(readConfigText(path));
}

function writeConfigObject(path: string, config: Json): void {
  mkdirSync(dirname(path), { recursive: true });
  const temporary = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, `${JSON.stringify(config, null, 2)}\n`, "utf8");
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
}

/** Validate a whole edited file and write it. */
export function writeConfigText(path: string, text: string): void {
  writeConfigObject(path, parseConfig(text));
}

function lookup(config: Json, path: string[]): unknown {
  let node: unknown = config;
  for (const key of path) {
    if (typeof node !== "object" || node === null || Array.isArray(node)) return undefined;
    node = (node as Json)[key];
  }
  return node;
}

/**
 * Set one key (undefined removes it), re-reading the file first so edits
 * made elsewhere since the page opened survive.
 */
export function setConfigValue(path: string, keyPath: string[], value: unknown): void {
  const config = readConfig(path);
  let node = config;
  for (const key of keyPath.slice(0, -1)) {
    const next = node[key];
    if (next === undefined && value === undefined) return;
    if (next === undefined) node[key] = {};
    else if (typeof next !== "object" || next === null || Array.isArray(next)) {
      throw new Error(`${key} in config.json is not an object`);
    }
    node = node[key] as Json;
  }
  const leaf = keyPath[keyPath.length - 1];
  if (value === undefined) delete node[leaf];
  else node[leaf] = value;
  writeConfigObject(path, config);
}

/** The value shown in the list for a key's current setting. */
export function displayValue(spec: SettingSpec, raw: unknown): string {
  if (raw === undefined || raw === null || raw === "") return spec.default;
  if (spec.kind === "secret") return "set";
  return typeof raw === "string" ? raw : JSON.stringify(raw);
}

/** Parse typed text into the stored value; undefined removes the key. */
export function parseInput(spec: SettingSpec, text: string): { value: unknown } | { error: string } {
  const trimmed = text.trim();
  if (trimmed === "") return { value: undefined };
  switch (spec.kind) {
    case "int":
      return /^\d+$/.test(trimmed) ? { value: Number(trimmed) } : { error: "Enter a whole number, or nothing for the default." };
    case "duration":
      return /^\d+[smhd]$/.test(trimmed) ? { value: trimmed } : { error: "Enter a duration such as 30m, 24h, 7d or 0s." };
    case "bool":
      return trimmed === "true" || trimmed === "false" ? { value: trimmed === "true" } : { error: "Enter true or false." };
    case "enum":
      return spec.values?.includes(trimmed) ? { value: trimmed } : { error: `Enter one of: ${spec.values?.join(", ")}.` };
    default:
      return { value: trimmed };
  }
}

export interface SettingsPageDeps {
  tui: PiTuiModule;
  theme: { fg(color: string, text: string): string };
  configPath: string;
  /** What the manager reported; null when it is unreachable or silent. */
  messaging?: MessagingStatus | null;
  enrollMessagingToken(token: string): Promise<void>;
  requestRender(): void;
  /** Report a save or its failure under the list. */
  notice(text: string, error?: boolean): void;
}

export function listTheme(theme: SettingsPageDeps["theme"]): PiTuiSettingsListTheme {
  return {
    label: (text, selected) => (selected ? theme.fg("accent", text) : text),
    value: (text, selected) => (selected ? theme.fg("accent", text) : theme.fg("muted", text)),
    description: (text) => theme.fg("dim", text),
    cursor: theme.fg("accent", "→ "),
    hint: (text) => theme.fg("dim", text),
  };
}

const keyOf = (spec: SettingSpec) => spec.path.join(".");

/** The token is removed after enrollment, so the row shows enrollment instead. */
function enrollmentValue(messaging: SettingsPageDeps["messaging"]): string {
  if (!messaging) return "unknown";
  return messaging.device_id ? `enrolled · device ${messaging.device_id.slice(0, 8)}` : "not enrolled";
}

function describe(spec: SettingSpec & { applies?: string }, section: SettingsSection, raw: unknown): string {
  const state = raw === undefined ? `not set (default ${spec.default})` : `default ${spec.default}`;
  return `${spec.description} · ${state} · ${spec.applies ?? section.applies}`;
}

/** One-line text editor for a single key. */
function valueEditor(
  deps: SettingsPageDeps,
  spec: SettingSpec,
  raw: unknown,
  save: (value: unknown) => boolean | Promise<boolean>,
  done: (shown?: string) => void,
): PiTuiInteractive {
  const input = new deps.tui.Input();
  input.focused = true;
  if (spec.kind !== "secret" && raw !== undefined && raw !== null) {
    // A paste leaves the cursor after the text; setValue would leave it at 0.
    input.handleInput(`\x1b[200~${typeof raw === "string" ? raw : JSON.stringify(raw)}\x1b[201~`);
  }
  const hint =
    spec.kind === "secret"
      ? "enter save · empty keeps the current value · esc cancel"
      : `enter save · empty resets to ${spec.default} · esc cancel`;
  let error = "";
  let pending = false;
  input.onSubmit = (text) => {
    if (spec.kind === "secret" && text.trim() === "") return done();
    const parsed = parseInput(spec, text);
    if ("error" in parsed) {
      error = parsed.error;
      deps.requestRender();
      return;
    }
    error = "";
    const saved = save(parsed.value);
    if (typeof saved === "boolean") {
      if (saved) done(displayValue(spec, parsed.value));
    } else {
      pending = true;
      deps.requestRender();
      void saved.then((success) => {
        pending = false;
        if (success) done("enrolled");
        deps.requestRender();
      });
    }
  };
  input.onEscape = () => done();
  return {
    render: (width) => [
      deps.theme.fg("accent", keyOf(spec)),
      deps.theme.fg("dim", spec.description),
      "",
      ...(pending ? [deps.theme.fg("dim", "Exchanging token...")] : input.render(width)),
      "",
      error ? deps.theme.fg("error", error) : deps.theme.fg("dim", pending ? "Waiting for enrollment response" : hint),
    ],
    invalidate: () => input.invalidate(),
    handleInput: (data) => { if (!pending) input.handleInput(data); },
  };
}

/** The list of one section's keys; esc returns to the overview. */
export function sectionPage(deps: SettingsPageDeps, section: SettingsSection, close: () => void): PiTuiInteractive {
  let config: Json;
  try {
    config = readConfig(deps.configPath);
  } catch (error) {
    deps.notice(`Cannot read ${deps.configPath}: ${(error as Error).message}`, true);
    config = {};
  }
  const specs = new Map(section.specs.map((spec) => [keyOf(spec), spec]));
  const enroll = async (token: string): Promise<boolean> => {
    try {
      await deps.enrollMessagingToken(token);
      setConfigValue(deps.configPath, ["messaging", "token"], undefined);
      deps.notice("Device enrolled; device credential saved.");
      return true;
    } catch (error) {
      deps.notice(`Device enrollment failed: ${(error as Error).message.replaceAll(token, "[redacted]")}`, true);
      return false;
    }
  };
  const save = (spec: SettingSpec & { applies?: string }, value: unknown): boolean | Promise<boolean> => {
    if (keyOf(spec) === "messaging.token") return enroll(value as string);
    try {
      setConfigValue(deps.configPath, spec.path, value);
    } catch (error) {
      deps.notice(`${keyOf(spec)} was not saved: ${(error as Error).message}`, true);
      return false;
    }
    deps.notice(`Saved ${keyOf(spec)}; ${spec.applies ?? section.applies}.`);
    return true;
  };
  const items: PiTuiSettingItem[] = section.specs.map((spec) => {
    const raw = lookup(config, spec.path);
    const isToken = keyOf(spec) === "messaging.token";
    const item: PiTuiSettingItem = {
      id: keyOf(spec),
      label: spec.path[spec.path.length - 1],
      description: isToken
        ? `Entering a token ${deps.messaging?.device_id ? "re-enrolls" : "enrolls"} this device. ${spec.description} · ${spec.applies}`
        : describe(spec, section, raw),
      currentValue: isToken ? enrollmentValue(deps.messaging) : displayValue(spec, raw),
    };
    if (spec.kind === "bool") item.values = ["true", "false"];
    else if (spec.kind === "enum") item.values = spec.values;
    else {
      item.submenu = (_current, done) =>
        valueEditor(deps, spec, lookup(readConfigOrEmpty(deps.configPath), spec.path), (value) => save(spec, value), done);
    }
    return item;
  });
  const list = new deps.tui.SettingsList(
    items,
    12,
    listTheme(deps.theme),
    (id, value) => {
      const spec = specs.get(id);
      if (!spec || (!spec.values && spec.kind !== "bool")) return;
      const parsed = parseInput(spec, value);
      if ("value" in parsed && !save(spec, parsed.value)) {
        list.updateValue(id, displayValue(spec, lookup(readConfigOrEmpty(deps.configPath), spec.path)));
      }
    },
    close,
  );
  return {
    render: (width) => [deps.theme.fg("accent", section.label), "", ...list.render(width)],
    invalidate: () => list.invalidate(),
    handleInput: (data) => list.handleInput(data),
  };
}

function readConfigOrEmpty(path: string): Json {
  try {
    return readConfig(path);
  } catch {
    return {};
  }
}
