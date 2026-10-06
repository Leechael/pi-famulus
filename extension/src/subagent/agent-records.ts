/**
 * On-disk agent child records so `pi-famulus ls` can see in-process subagents
 * (design doc §4.3: task_list merges manager tasks with subagent runs).
 *
 * Layout: <home>/sessions/<session_id>/agents/<child_id>.json
 */
import { mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
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
  /** Provider-reported cumulative token counts (input includes cached prompt tokens). */
  tokens_input?: number;
  tokens_output?: number;
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

/** Write one child record (mkdir + overwrite). */
export function writeAgentChildRecord(home: string, record: AgentChildRecord): void {
  const dir = agentRecordsDir(home, record.session_id);
  mkdirSync(dir, { recursive: true });
  writeFileSync(agentRecordPath(home, record.session_id, record.child_id), JSON.stringify(record));
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
