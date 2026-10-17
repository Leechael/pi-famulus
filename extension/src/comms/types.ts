/**
 * comms types (design doc appendix B, M4).
 *
 * `ChildStatus` / `ChildResult` / `ChildHandle` are structural re-declarations
 * of the appendix B contract. They are intentionally NOT imported from
 * `../subagent/types` (M3 is developed in parallel and may not exist yet);
 * any value satisfying these shapes is accepted.
 */

// ---------- structural subagent types (appendix B, re-declared) ----------

export type ChildStatus = "pending" | "running" | "completed" | "failed" | "interrupted";

export interface ChildResult {
  status: "completed" | "failed" | "interrupted";
  text: string;
  error?: string;
  durationMs: number;
}

export interface ChildHandle {
  readonly childId: string;
  readonly result: Promise<ChildResult>;
  steer(message: string): Promise<void>;
  followUp(message: string): Promise<void>;
  resume(message: string): Promise<void>;
  interrupt(): Promise<void>;
  status(): ChildStatus;
  lastEventAt(): number;
  /** Resolved `provider/id` once the child session exists (optional for stubs). */
  resolvedModel?(): string | undefined;
}

import type { FamulusWake } from "../wake";

// ---------- comms contract (appendix B, verbatim) ----------

export interface CommsHost {
  // Implemented by the subagent registry (wired at integration time).
  getChild(
    childId: string,
  ): { handle: ChildHandle; runId: string; name: string; status: ChildStatus } | undefined;
  listChildren(): { childId: string; runId: string; name: string; status: ChildStatus }[];
  sameRun(childIdA: string, childIdB: string): boolean;
  notifySupervisor(wake: { content: string; details: FamulusWake }): void;
  /** Continue an idle child. Absent on test hosts that still reject send-to-finished. */
  resumeChild?(childId: string, message: string): Promise<void>;
  /** Fired after resumeChild accepts a turn, so the parent can be woken when it settles. */
  onResumeAccepted?(childId: string): void;
}

export interface MailboxEntry {
  ts: number;
  from: string; // "supervisor" | childId
  to: string; // "supervisor" | childId
  kind: "need_decision" | "progress_update" | "send" | "reply" | "broadcast";
  message: string;
  reply?: string;
}

export interface Comms {
  contactSupervisor(
    fromChildId: string,
    reason: "need_decision" | "progress_update",
    message: string,
  ): Promise<string>; // need_decision → reply text; progress_update → immediate "ok"
  reply(toChildId: string, message: string): void; // no pending waiter → throw
  send(toChildId: string, message: string, delivery: "steer" | "queue"): Promise<void>;
  broadcast(runId: string, message: string, fromChildId?: string): Promise<string[]>; // delivered childIds
  pendingRequests(): { childId: string; name: string; message: string; sinceMs: number }[];
  log(runId: string, limit?: number): MailboxEntry[]; // default 20, ring 200/run
}
