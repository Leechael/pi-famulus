/**
 * Types for the pi-famulus intercom port.
 *
 * Upstream baseline: agent-intercom @ 98183fb, plugin/pi (the pi plugin's
 * observable behaviour — tools, commands, envelopes, receipts, busy/idle
 * publication — is preserved; the client.sock transport is replaced by the
 * famulus manager connection, protocol 7 `intercom` request + events).
 */

/** A directory row as the bridge publishes it. */
export interface SessionRow {
  id?: string;
  name?: string;
  harness?: string;
  status?: string;
  cwd?: string;
  updated_at?: number;
  machine_id?: string;
  machine_name?: string;
}

/** One client-protocol frame pushed by the bridge (`intercom` event). */
export interface IntercomFrame {
  type?: string;
  session_id?: string;
  from?: string;
  body?: string;
  reply_to?: string;
  message_id?: string;
  status?: string;
  subscription_id?: string;
  sessions?: SessionRow[];
  error?: string;
}

/** Machine a session runs on: its display name, else its machine id. */
export function hostOf(row: SessionRow): string {
  return (row.machine_name || row.machine_id || "").trim();
}
