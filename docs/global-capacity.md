# Machine-wide agent capacity

The daemon owns `maxAgents` in `<home>/config.json` (default 8). Change it at runtime with `pi-famulus config set max-agents 12`; `config get max-agents` reads the same setting. Protocol 5 adds optional per-work-kind budgets. The supported CLI names and JSON fields are:

| CLI key | Config field | Default |
|---|---|---:|
| `max-test-suite` | `maxTestSuite` | 2 |
| `max-test` | `maxTest` | 2 |
| `max-build` | `maxBuild` | current `maxAgents` |
| `max-lint/type` | `maxLintType` | current `maxAgents` |
| `max-other` | `maxOther` | current `maxAgents` |
| `max-git` | `maxGit` | current `maxAgents` |
| `max-read/search` | `maxReadSearch` | current `maxAgents` |

Unconfigured kinds inherit the global limit, except `test-suite` and `test`, which default to 2. Every configured value must be a positive integer. `status` reports global used/total and used/total for each kind; JSON status includes the same data under `agent_capacity.by_kind`. Successful changes append a `capacity.changed` event and notify a running daemon, which re-evaluates eligible waiters. Invalid or non-object `config.json` is surfaced as an error; admission never silently falls back to a larger budget.

## Work-kind source

`subagent({tasks:[...]})` items and `chain` steps may declare `work_kind` from `test-suite`, `test`, `build`, `lint/type`, `other`, `git`, or `read/search`; the default is `other`. This is an explicit planned-work annotation, not a guess from the natural-language prompt. That is the honest spawn-time boundary: `work-index.ts` and `task-tools.ts` know only that an item is an `agent`, while `manager/src/workkind.rs` classifies actual shell command text for task inspection/statistics. A subagent's future commands do not exist when its admission request is made, so prompt/name heuristics would claim evidence the extension does not have. The explicit label is retained as agent work-index metadata and in agent records/events; task-list rows keep their existing coarse `[agent]` label for compatibility. Shell-task `workkind.rs` classification remains unchanged.

## Admission and queue lifecycle

A v5 `acquire_agent(child_id, work_kind)` is idempotent for the child-owned permit. The extension retries a queued acquire every 3 seconds with the same request id. If a wake grant could not enter the bounded response channel, the daemon keeps its granted state and retries delivery on that session's next request; the repeated acquire also returns the existing grant. The extension's cancellation request includes the child and acquire request ids; the daemon only releases a held permit when both match its owner. If the machine-wide budget is already full, it preserves the protocol-4 `global_capacity` rejection. When global capacity is available but this kind is full, the daemon holds the original request id, response writer, session id, child id, and kind in a bounded FIFO pending queue (64 distinct children per session, 256 daemon-wide). Overflow is rejected as `capacity_queue_full`; up to eight request ids for an already-pending child attach to its existing waiter rather than consume another slot, and further aliases are rejected as `capacity_queue_full`. Queued requests of each kind are granted in arrival order when both global and kind capacity allow them. A request in another kind with available budgets may be admitted independently rather than being head-of-line blocked by a waiter for a full kind. Release or a runtime budget increase scans for the earliest eligible waiter, sending an ordinary response with its original id so the extension's normal multiplexed pending promise resolves.

Budget reductions never revoke active permits and do not remove/reorder pending requests. A grant is made only if both budgets have room at wake time, so after shrinking below current use the queued requests remain FIFO until active permits drain below the new budget. The extension keeps its local reservation while awaiting a queued global response. If its child is interrupted/disposed while queued, the generation aborts the request and sends `cancel_acquire_agent(request_id, child_id)`; the daemon atomically removes a matching pending request (or an acquire that won the grant race), and fills any newly free slot from the remaining queue. A stale or duplicate cancellation id is a no-op. If cancellation races ahead of an acquire frame, its one-shot tombstone consumes that frame; a later retry with the same request id is evaluated as a new acquire. On socket close/rebind, connection-bound pending requests are discarded. On a full session disconnect, that session's granted permits are also reaped. A daemon restart resets permits; the extension re-registers its held `(child_id, work_kind)` leases after reconnect.

`queue_ms` is attributed from the child entering pending admission until it actually starts, including time waiting for local/global admission. It is written to `agent.start`, persisted agent records, and the child work-index `WorkItem.queueMs`; the usual `duration_ms` still includes queue wait. Registry admission owns the full queue interval, so runner-reported wait is not added to it a second time.

## Protocol compatibility

Hello advertises a client's maximum feature level; the server hello response reports the daemon maximum. The extension uses `min(its v5 maximum, daemon maximum)` as the effective protocol. A manager below v5 is used with protocol-4 acquire requests: no kind field, no per-kind limit, no queueing, and an immediate `global_capacity` rejection when the machine budget is full. A v5 manager applies the same v4 behavior to sessions that announced protocol <5. Below protocol 4, or if the manager is unavailable, the extension falls back to its existing per-session child limit and emits one notice per reason.

The permit is owned by `(session_id, child_id)` and lasts until terminal settle (completed, failed, or disposed). An interrupted-but-resumable child keeps its permit; resume reserves a local slot but does not reacquire globally. Release is idempotent. The extension reserves its per-session `maxConcurrentChildren` slot before requesting a machine permit and releases that local reservation while retrying the protocol-4 global-capacity rejection path.
