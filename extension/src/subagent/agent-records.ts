/**
 * On-disk agent child records so `pi-famulus ls` can see in-process subagents
 * (design doc §4.3: task_list merges manager tasks with subagent runs).
 *
 * Layout: <home>/sessions/<session_id>/agents/<child_id>.json
 */
import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, readdirSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export interface AgentChildRecord {
  v: 1;
  kind: "agent";
  child_id: string;
  run_id: string;
  session_id: string;
  name: string;
  agent: string;
  /** Explicit planned work class; old records default to `other`. */
  work_kind?: string;
  /** Time spent waiting for admission, when measured. */
  queue_ms?: number;
  /** Resolved model id when known (param override or agent definition). */
  model?: string;
  status: "pending" | "running" | "completed" | "failed" | "interrupted";
  /** Total generations run (1 + resumes + stall retries); omitted when 1. */
  attempts?: number;
  /** Stall detections in the final user turn. */
  stalls?: number;
  started_at: number;
  ended_at?: number;
  error?: string;
  /** Why the child ended (terminal statuses only). */
  end_reason?: AgentEndReason;
  /** Task prompt without the agent preamble, capped. */
  prompt_head?: string;
  /** Tail of the final result text, capped. */
  result_tail?: string;
  /** Tool results seen in the transcript so far. */
  tool_calls?: number;
  /** Cumulative provider usage.input and usage.output as reported. */
  tokens_input?: number;
  tokens_output?: number;
  /** Separate cumulative provider cache counters; not folded into tokens_input. */
  tokens_cache_read?: number;
  tokens_cache_write?: number;
  /** Observed assistant-message, tool-execution, and admission-queue wall time. */
  llm_ms?: number;
  tool_ms?: number;
  /** Wall time that did not fit the observed phase boundaries. */
  wall_other_ms?: number;
  wall_approximate?: boolean;
  /** Absolute path of the live `<child_id>.jsonl` transcript. */
  transcript?: string;
}

export type AgentEndReason =
  | "completed"
  | "failed"
  | "model-error"
  | "stalled"
  | "timeout"
  | "interrupted"
  | "disposed";

/** Map a terminal child status + result to the end_reason recorded on disk. */
export function agentEndReason(
  status: AgentChildRecord["status"],
  result: { error?: string; endReason?: "model-error" } | undefined,
): AgentEndReason | undefined {
  if (status === "completed") return "completed";
  if (status === "failed") {
    if (result?.endReason === "model-error") return "model-error";
    return result?.error === "stalled" ? "stalled" : "failed";
  }
  if (status === "interrupted") {
    if (result?.error === "timeout") return "timeout";
    if (result?.error === "disposed") return "disposed";
    return "interrupted";
  }
  return undefined;
}

const HEAD_TAIL_CHARS = 2000;

export function headOf(text: string): string {
  return text.length <= HEAD_TAIL_CHARS ? text : `${text.slice(0, HEAD_TAIL_CHARS)}…`;
}

export function tailOf(text: string): string {
  return text.length <= HEAD_TAIL_CHARS ? text : `…${text.slice(-HEAD_TAIL_CHARS)}`;
}

export function agentRecordsDir(home: string, sessionId: string): string {
  return join(home, "sessions", sessionId, "agents");
}

export function agentRecordPath(home: string, sessionId: string, childId: string): string {
  return join(agentRecordsDir(home, sessionId), `${childId}.json`);
}

/** Write one child record atomically (mkdir + same-directory temp file + rename). */
export function writeAgentChildRecord(home: string, record: AgentChildRecord): void {
  const dir = agentRecordsDir(home, record.session_id);
  mkdirSync(dir, { recursive: true });
  const path = agentRecordPath(home, record.session_id, record.child_id);
  const tempPath = `${path}.${process.pid}.${randomBytes(6).toString("hex")}.tmp`;
  try {
    writeFileSync(tempPath, JSON.stringify(record), { flag: "wx" });
    renameSync(tempPath, path);
  } finally {
    try {
      unlinkSync(tempPath);
    } catch {
      // rename consumed the temporary file, or a failed write left none.
    }
  }
}

export type AgentChildTelemetry = Partial<Pick<AgentChildRecord,
  "tokens_input" | "tokens_output" | "tokens_cache_read" | "tokens_cache_write" | "llm_ms" | "tool_ms" | "queue_ms" | "wall_other_ms" | "wall_approximate"
>>;

/** Refresh live telemetry without waiting for a child state transition. */
export function updateAgentChildMetrics(
  home: string,
  sessionId: string,
  childId: string,
  metrics: AgentChildTelemetry,
): void {
  const path = agentRecordPath(home, sessionId, childId);
  try {
    const record = JSON.parse(readFileSync(path, "utf8")) as Partial<AgentChildRecord>;
    if (record.v !== 1 || record.kind !== "agent" || record.session_id !== sessionId || record.child_id !== childId) return;
    writeAgentChildRecord(home, { ...(record as AgentChildRecord), ...metrics });
  } catch {
    // A metrics refresh must never interfere with the in-process child.
  }
}

export function updateAgentChildTokens(
  home: string,
  sessionId: string,
  childId: string,
  tokens: { input: number; output: number; cacheRead: number; cacheWrite: number },
): void {
  updateAgentChildMetrics(home, sessionId, childId, {
    tokens_input: tokens.input,
    tokens_output: tokens.output,
    tokens_cache_read: tokens.cacheRead,
    tokens_cache_write: tokens.cacheWrite,
  });
}

export function removeAgentChildRecord(home: string, sessionId: string, childId: string): void {
  try {
    unlinkSync(agentRecordPath(home, sessionId, childId));
  } catch {
    // already gone
  }
}

function parseRecord(raw: string): AgentChildRecord | null {
  try {
    const v = JSON.parse(raw) as Partial<AgentChildRecord>;
    if (v.v !== 1 || v.kind !== "agent") return null;
    if (typeof v.child_id !== "string" || typeof v.session_id !== "string") return null;
    if (typeof v.name !== "string" || typeof v.agent !== "string") return null;
    if (typeof v.status !== "string" || typeof v.started_at !== "number") return null;
    return v as AgentChildRecord;
  } catch {
    return null;
  }
}

/** Load agent child records from disk (all sessions under home). */
export function loadAgentChildRecords(
  home: string,
  opts: { sessionId?: string; includeTerminal?: boolean; connectedSessionIds?: ReadonlySet<string> } = {},
): AgentChildRecord[] {
  const sessionsRoot = join(home, "sessions");
  let sessionIds: string[];
  try {
    sessionIds = opts.sessionId
      ? [opts.sessionId]
      : readdirSync(sessionsRoot, { withFileTypes: true })
          .filter((d) => d.isDirectory())
          .map((d) => d.name);
  } catch {
    return [];
  }
  const out: AgentChildRecord[] = [];
  for (const sid of sessionIds) {
    const dir = agentRecordsDir(home, sid);
    let files: string[];
    try {
      files = readdirSync(dir).filter((f) => f.endsWith(".json"));
    } catch {
      continue;
    }
    for (const file of files) {
      try {
        const rec = parseRecord(readFileSync(join(dir, file), "utf8"));
        if (!rec) continue;
        const disconnected =
          opts.connectedSessionIds !== undefined && !opts.connectedSessionIds.has(rec.session_id);
        const shown: AgentChildRecord =
          disconnected && (rec.status === "pending" || rec.status === "running")
            ? { ...rec, status: "interrupted" }
            : rec;
        const terminal =
          shown.status === "completed" || shown.status === "failed" || shown.status === "interrupted";
        if (!opts.includeTerminal && terminal) continue;
        out.push(shown);
      } catch {
        // skip bad files
      }
    }
  }
  return out;
}

export function isAgentStatusActive(status: AgentChildRecord["status"]): boolean {
  return status === "pending" || status === "running";
}

/** Display command column for CLI / task_list. */
export function formatAgentCommand(rec: AgentChildRecord): string {
  const model = rec.model ? ` ${rec.model}` : "";
  return `agent:${rec.name} (${rec.agent})${model}`;
}
