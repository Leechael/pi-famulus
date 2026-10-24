/**
 * The intercom transport: one op at a time onto the famulus manager
 * connection, frames pushed back as `intercom` events.
 *
 * Upstream baseline: agent-intercom @ 98183fb, plugin/pi — this interface
 * replaces the plugin's Unix socket (`client.sock`) with the manager
 * connection the extension already holds (`manager.sock`, protocol 7).
 */

import { ManagerClient, ManagerError } from "../manager-client";

/** Why the bridge cannot serve ops right now. */
export type BridgeDownReason = "disabled" | "unavailable";

export class IntercomError extends Error {
  constructor(
    message: string,
    readonly code?: string,
    readonly reason?: BridgeDownReason,
  ) {
    super(message);
    this.name = "IntercomError";
  }
}

export interface IntercomTransport {
  /** Issue one intercom op; rejects with IntercomError when the bridge or
   * the manager connection is down. Op-level errors resolve with the
   * daemon's error object (`{ error: string }`). */
  call(op: Record<string, unknown>): Promise<Record<string, unknown>>;
  /** Bridge frames for this session (deliver/receipt/idle_notice). */
  onFrame(handler: (frame: Record<string, unknown>) => void): () => void;
  /** Registration is invalidated: bridge restart (`intercom_reset`) or the
   * manager connection re-hello'd (restart / in-place upgrade). */
  onReset(handler: () => void): () => void;
  /** The manager connection is up (registration may still be required). */
  isUp(): boolean;
}

const DOWN_CODES: Record<string, BridgeDownReason> = {
  E_INTERCOM_DISABLED: "disabled",
  E_INTERCOM_UNAVAILABLE: "unavailable",
};

/**
 * The transport over the extension's ManagerClient. One instance per pi
 * session runtime; `getClient` resolves the current session's client, which
 * is (re)created on session_start and torn down on shutdown.
 */
export function createManagerTransport(
  getClient: () => ManagerClient | null,
): IntercomTransport {
  return {
    async call(op) {
      const client = getClient();
      if (!client || !client.isAvailable()) {
        throw new IntercomError("pi-famulus is not connected", "E_TRANSPORT", "unavailable");
      }
      try {
        return await client.intercom(op);
      } catch (error) {
        if (error instanceof ManagerError) {
          const reason = DOWN_CODES[error.code];
          if (reason) throw new IntercomError(error.message, error.code, reason);
          // Op-level failure (unknown session, ownership ended, …): the
          // caller reads the `error` field, like the plugin's frame did.
          return { error: error.message, code: error.code };
        }
        throw error instanceof IntercomError
          ? error
          : new IntercomError(error instanceof Error ? error.message : String(error));
      }
    },
    onFrame(handler) {
      const client = getClient();
      if (!client) return () => {};
      return client.onEvent((event) => {
        if (event.event === "intercom" && event.frame && typeof event.frame === "object") {
          handler(event.frame);
        }
      }) ?? (() => {});
    },
    onReset(handler) {
      const client = getClient();
      if (!client) return () => {};
      const offEvent = client.onEvent((event) => {
        if (event.event === "intercom_reset") handler();
      }) ?? (() => {});
      // A reconnect (daemon restart or in-place upgrade) re-hellos the same
      // manager socket; the port does not survive that either.
      const offReconnect = client.onReconnect(handler) ?? (() => {});
      return () => {
        offEvent();
        offReconnect();
      };
    },
    isUp() {
      const client = getClient();
      return Boolean(client && client.isAvailable());
    },
  };
}
