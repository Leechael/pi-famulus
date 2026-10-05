# Stall auto-resume

**Status:** Accepted. Implemented in `feat/stall-auto-resume`.
**Decision:** A stall is transient, not terminal. The runner aborts the hung generation and auto-resumes it on the **same session** with a continuation prompt, up to `stallRetries` times (default 1, delay `stallRetryDelayMs` default 5s). Only when the retry budget is spent does the run settle as `failed (stalled)`.
**Inciting incident:** dogfooding session 2026-09-27 — a foreground subagent on `kimi-coding/kimi-for-coding` emitted one assistant message, then the provider stream stalled silently: no error, no close, no tool call. Neither the provider layer (no per-chunk read timeout) nor pi-subagents (activity timer only refreshes the UI; nothing compares time-since-last-event) cut it short. The child sat silent for 17 minutes until manually interrupted; the partial findings were lost.

## Why a retry, and why on the same session

The transcript before the stall is real work: reasoning, read results, edits. Throwing the session away repeats all of it and can double-apply side effects. Resuming on the same session keeps the transcript, the admission slot, and the result promise — the caller cannot tell a retried completion from a clean one except via the new `attempts` field on `ChildResult` (and the per-detection `agent.stall {child_id, attempt}` event).

A stall mid-response is also almost always provider flakiness, not a task property: the parent session's own turns on the same provider kept working. One bounded retry converts the observed failure ("17-min silent hang → manual interrupt → lost work") into a 5-minute hiccup.

## Alternatives considered

- **Surface-only (status quo):** settle `failed (stalled)` and let the orchestrator resume manually. Rejected: the incident showed orchestrators don't act on stalls they aren't woken about; the run result is what they consume. The stall event now fires at *detection* time (not at settle), so a watching orchestrator still can act.
- **New session per retry:** clean, but discards the transcript and re-runs completed work; tool side effects (edits, starts) would re-execute from scratch.
- **Provider-level stream read timeout:** correct fix for the root cause, but lives in pi core / the kimi provider package — out of this repo's reach. The runner-level watchdog stays valuable as defense-in-depth even if a provider timeout lands later.
- **Retry timeouts and model errors too:** rejected. `timeout` is a deliberate budget (auto-retry would defeat it), and a model error already settled the turn — re-prompting a provider that just returned an error is a different failure class.

## Semantics worth pinning

- **Retired generation:** at detection the stalled generation is *retired*, not settled. Its aborted `prompt()` promise resolves shortly after; without a `retiredGen` marker, `finishGeneration` would report that abort-resolution as the run's completion while the retry is still pending. `isCurrent()` excludes retired generations; the retry timer and `settle()` deliberately do not.
- **The retry awaits the abort (review finding B1).** Real pi rejects `prompt()` with "Agent is already processing" while the aborted run is still active (`agent-session.js`), so re-prompting before the abort finishes wastes the retry in the exact incident case. The retry waits for `abort()` bounded by `stallMs`; an abort that never completes means the hung stream ignored it — the session is dead and the run settles `failed (stalled)` without retrying. The fake session now mirrors pi (prompt rejects while streaming; `hungAbort`/`abortGate` modes), and the B1 tests were proven red against a build with the wait neutered.
- **`timeoutMs` is a per-user-turn budget, not per generation (S1).** *Amended by `subagent-soft-deadline.md`.* `timeoutMs` no longer aborts. The soft deadline keeps the existing turn schedule (`softDeadlineAt` / `nextReminderAt`); stall retries re-arm those timers rather than a remaining-time budget, and an overrun during a retry delay sends a wake without stopping the retry. The legacy S1 behaviours — stall retries arm only the remaining time, and a timeout landing during a retry delay still fires — now apply only to the opt-in `hardTimeoutMs`. That hard-ceiling guard deliberately does not use `isCurrent()`, otherwise the retired generation would mask a spent ceiling until the retry finished. A user `resume()` starts a new turn with a fresh budget.
- **The budget clock starts after admission, not at launch (R1, second review).** The budget anchor (`turnBudgetStart`) is set once admission and session creation complete — time spent queued for an admission slot is not the child's budget. `runStartedAt` (for `durationMs`) still starts at launch, so queue time remains visible in forensics. Worst-case recovery from a stall is therefore `stallRetryDelayMs + stallMs` (the bounded abort wait) on top of detection.
- **`durationMs` covers the whole user turn** (launch/resume → settle), including stall and delay time — the incident class this exists for needs that number. `attempts` still counts user resumes; the per-turn stall count is the separate `stalls` field (S2/S4).
- **The stall budget belongs to a user turn (S3):** `resume()` resets `stallAttempts`, so a later turn gets its own `stallRetries`. `agent.stall`'s `attempt` numbering restarts accordingly.
- **`steer()`/`followUp()` during the restart window reject** with "restarting after a stall" — delivering into an aborted session would be undefined (N1).
- **Interrupt during the retry delay** cancels the retry (existing `interrupt()` semantics; result `interrupted`).
- **Admission slot** is held across the retry (`reuseSlot`): no re-acquire, exactly-once release — the guarantee the Effect pilot ADR called out as the contract to keep.
- **Per-tool timeouts are unaffected:** the stall watchdog stays paused during `tool_execution_start`…`end` and pending `need_decision`, and the retry re-arms it for the new generation.
- **`stallRetries: 0`** restores the previous terminal-stall behavior; existing tests pin both modes.

## Evidence

- `extension/tests/unit/impl-subagent-runner.test.ts` — incident-shaped regression (text → silence → abort → auto-resume → complete), exhausted budget, interrupt/dispose during the delay, slot retention, watchdog re-arm, retired-generation no-false-completion, abort-await (gate and hung-abort), timeout-during-delay and remaining-budget arithmetic, resume budget reset, restart-window steer/followUp rejection, and late-event tolerance.
- `extension/tests/unit/impl-subagent-config.test.ts` — defaults, section parsing, `0` = legacy mode, sanitization.
- Independent review (1 blocker, 5 suggestions, 3 nits) addressed in the second commit; the blocker came with a red-test proof requirement, done by neutering the abort wait.
- `npm test`: 396 passed; `tsc --noEmit` clean.
