/**
 * The ONLY module that knows the wire format of injected wake messages.
 *
 * Contract: docs/design.md §4.5 "pi-famulus-wake contract". One customType (`pi-famulus-wake`),
 * content = FAMULUS_WAKE_LEAD_IN + blank line + one <pi-famulus-wake kind="…"> element,
 * and a camelCase `details` object discriminated by `kind`.
 *
 * `details` is authoritative when the event stream carries it (pi puts it on
 * the custom message); the XML is parsed only as a fallback (e.g. contexts
 * that keep content but drop details). Graders consume the normalized `Wake`.
 */
import { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN } from "../../extension/src/wake.ts";

export { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN };

export type WakeKind =
  | "task"
  | "monitor"
  | "subagent-handover"
  | "subagent-done"
  | "subagent-overrun"
  | "supervisor-request"
  | "supervisor-update"
  | "unknown";

export interface WakeItem {
  id: string;
  title: string;
}

export interface WakeTask {
  id: string;
  taskKind: string;
  status: string;
  exitCode: number | null;
  /** Signal name ("SIGTERM"); numeric signals from older managers are mapped to names. */
  signal?: string;
  summary: string;
  command: string;
  outputPath: string;
  preview: string;
  durationMs: number;
}

export interface WakeChild {
  childId: string;
  name: string;
  status: string;
  prompt: string;
  result: string;
  error?: string;
}

export interface Wake {
  kind: WakeKind;
  customType: string;
  /** Full injected text. */
  raw: string;
  /** Text before the <pi-famulus-wake> element (the shared lead-in, or "" if ablated). */
  leadIn: string;
  /** Where the fields came from. */
  source: "details" | "xml";
  /** Shell/monitor task ids carried by this wake. */
  taskIds: string[];
  /** task: comma-joined task statuses; monitor: status or "event"; subagent: run/child status. */
  status?: string;
  tasks: WakeTask[];
  runId?: string;
  childId?: string;
  childName?: string;
  children: WakeChild[];
  stillRunning: WakeItem[];
  /** Payload text: previews, monitor event, handover result, done results, supervisor message. */
  body: string;
  /** supervisor-request: the reply recipe (<reply-with>). */
  replyWith?: string;
}

export function isWakeCustomType(customType: string | undefined): boolean {
  return customType === FAMULUS_WAKE_CUSTOM_TYPE;
}

const SIGNALS: Record<number, string> = { 1: "SIGHUP", 2: "SIGINT", 9: "SIGKILL", 15: "SIGTERM" };

function signalName(v: unknown): string | undefined {
  if (v === undefined || v === null || v === "") return undefined;
  if (typeof v === "number" || /^\d+$/.test(String(v))) return SIGNALS[Number(v)] ?? `SIG${v}`;
  return String(v);
}

// ---------------------------------------------------------------------------
// XML helpers (fallback path)
// ---------------------------------------------------------------------------

function unescapeXml(text: string): string {
  return text.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&amp;/g, "&");
}

function elements(text: string, name: string): Array<{ attrs: Record<string, string>; inner: string }> {
  const re = new RegExp(`<${name}((?:\\s+[\\w-]+="[^"]*")*)\\s*>([\\s\\S]*?)</${name}>`, "g");
  return [...text.matchAll(re)].map((m) => ({ attrs: parseAttrs(m[1]), inner: m[2] }));
}

function parseAttrs(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of s.matchAll(/([\w-]+)="([^"]*)"/g)) out[m[1]] = unescapeXml(m[2]);
  return out;
}

function child(text: string, name: string): string | undefined {
  const e = elements(text, name)[0];
  return e ? unescapeXml(e.inner) : undefined;
}

function stillRunningXml(text: string): WakeItem[] {
  const block = elements(text, "still-running")[0];
  if (!block) return [];
  return elements(block.inner, "item").map((i) => ({ id: i.attrs.id ?? "", title: unescapeXml(i.inner) }));
}

// ---------------------------------------------------------------------------

type Details = Record<string, unknown> & { kind?: string };

function blank(customType: string, raw: string, leadIn: string, source: Wake["source"]): Wake {
  return { kind: "unknown", customType, raw, leadIn, source, taskIds: [], tasks: [], children: [], stillRunning: [], body: "" };
}

function fromDetails(customType: string, raw: string, leadIn: string, d: Details): Wake {
  const w = blank(customType, raw, leadIn, "details");
  const items = (v: unknown) => ((v as WakeItem[] | undefined) ?? []).map((i) => ({ id: String(i.id), title: String(i.title) }));
  switch (d.kind) {
    case "task": {
      const tasks = ((d.tasks as Array<Record<string, unknown>>) ?? []).map((t) => ({
        id: String(t.id),
        taskKind: String(t.taskKind ?? "shell"),
        status: String(t.status),
        exitCode: typeof t.exitCode === "number" ? t.exitCode : null,
        ...(signalName(t.signal) ? { signal: signalName(t.signal) } : {}),
        summary: String(t.summary ?? ""),
        command: String(t.command ?? ""),
        outputPath: String(t.outputPath ?? ""),
        preview: String(t.preview ?? ""),
        durationMs: Number(t.durationMs ?? 0),
      }));
      return { ...w, kind: "task", tasks, taskIds: tasks.map((t) => t.id), status: tasks.map((t) => t.status).join(","), stillRunning: items(d.stillRunning), body: tasks.map((t) => t.preview).join("\n") };
    }
    case "monitor":
      return { ...w, kind: "monitor", taskIds: [String(d.id)], status: (d.status as string | undefined) ?? "event", body: String(d.event ?? "") };
    case "subagent-handover":
      return {
        ...w,
        kind: "subagent-handover",
        runId: String(d.runId),
        childId: String(d.childId),
        childName: String(d.name),
        status: String(d.status),
        stillRunning: items(d.stillRunning),
        body: String(d.result ?? ""),
      };
    case "subagent-done": {
      const children = ((d.children as Array<Record<string, unknown>>) ?? []).map((c) => ({
        childId: String(c.childId),
        name: String(c.name),
        status: String(c.status),
        prompt: String(c.prompt ?? ""),
        result: String(c.result ?? ""),
        ...(c.error ? { error: String(c.error) } : {}),
      }));
      return { ...w, kind: "subagent-done", runId: String(d.runId), status: String(d.status), children, body: children.map((c) => `## ${c.name}\n${c.result}`).join("\n\n") };
    }
    case "subagent-overrun":
      // The child is still running; the wake asks the parent to decide.
      return { ...w, kind: "subagent-overrun", runId: String(d.runId), childId: String(d.childId), childName: String(d.name), status: "running", body: String(d.summary ?? "") };
    case "supervisor-request":
    case "supervisor-update": {
      const replyWith = d.kind === "supervisor-request" ? child(raw, "reply-with") : undefined;
      return { ...w, kind: d.kind, childId: String(d.from), childName: String(d.name), body: String(d.message ?? ""), ...(replyWith ? { replyWith } : {}) };
    }
  }
  return w;
}

function fromXml(customType: string, raw: string, leadIn: string): Wake {
  const w = blank(customType, raw, leadIn, "xml");
  // The lead-in mentions a bare "<pi-famulus-wake>"; the envelope always carries kind="…".
  const root = /<pi-famulus-wake(\s+kind="[^"]*"(?:\s+[\w-]+="[^"]*")*)\s*>([\s\S]*)<\/pi-famulus-wake>/.exec(raw);
  if (!root) return w;
  const a = parseAttrs(root[1]);
  const inner = root[2];
  switch (a.kind) {
    case "task": {
      const tasks = elements(inner, "task").map((t) => ({
        id: t.attrs.id ?? "",
        taskKind: t.attrs.kind ?? "shell",
        status: t.attrs.status ?? "",
        exitCode: t.attrs["exit-code"] !== undefined ? Number(t.attrs["exit-code"]) : null,
        ...(signalName(t.attrs.signal) ? { signal: signalName(t.attrs.signal) } : {}),
        summary: child(t.inner, "summary") ?? "",
        command: child(t.inner, "command") ?? "",
        outputPath: child(t.inner, "output-file") ?? "",
        preview: child(t.inner, "preview") ?? "",
        durationMs: Number(t.attrs["duration-ms"] ?? 0),
      }));
      return { ...w, kind: "task", tasks, taskIds: tasks.map((t) => t.id), status: tasks.map((t) => t.status).join(","), stillRunning: stillRunningXml(inner), body: tasks.map((t) => t.preview).join("\n") };
    }
    case "monitor":
      return { ...w, kind: "monitor", taskIds: a.id ? [a.id] : [], status: a.status ?? "event", body: child(inner, "event") ?? "" };
    case "subagent-handover":
      return { ...w, kind: "subagent-handover", runId: a["run-id"], childId: a["child-id"], childName: a.name, status: a.status, stillRunning: stillRunningXml(inner), body: child(inner, "result") ?? "" };
    case "subagent-done": {
      const children = elements(inner, "child").map((c) => ({
        childId: c.attrs.id ?? "",
        name: c.attrs.name ?? "",
        status: c.attrs.status ?? "",
        prompt: child(c.inner, "prompt") ?? "",
        result: child(c.inner, "result") ?? "",
        ...(child(c.inner, "error") ? { error: child(c.inner, "error") } : {}),
      }));
      return { ...w, kind: "subagent-done", runId: a["run-id"], status: a.status, children, body: children.map((c) => `## ${c.name}\n${c.result}`).join("\n\n") };
    }
    case "subagent-overrun":
      return { ...w, kind: "subagent-overrun", runId: a["run-id"], childId: a["child-id"], childName: a.name, status: "running", body: child(inner, "summary") ?? "" };
    case "supervisor-request":
    case "supervisor-update": {
      const replyWith = child(inner, "reply-with");
      return { ...w, kind: a.kind, childId: a.from, childName: a.name, body: child(inner, "message") ?? "", ...(replyWith ? { replyWith } : {}) };
    }
  }
  return w;
}

/** Normalize one injected wake. Pass `details` whenever the source has it. */
export function parseWake(customType: string, content: string, details?: unknown): Wake {
  const at = content.search(/<pi-famulus-wake\s+kind="/);
  const leadIn = (at >= 0 ? content.slice(0, at) : content).trim();
  const d = details as Details | undefined;
  if (d && typeof d === "object" && typeof d.kind === "string") return fromDetails(customType, content, leadIn, d);
  return fromXml(customType, content, leadIn);
}
