# Parked hosted child resumes after its parent reports it finished

Status: **root cause confirmed; fixed on `fix/hosted-resume-turn-lifecycle` and
verified against the transport replay and a real daemon/RPC child**.

Investigated on `main` at `b9293da` (after `git pull --ff-only`). The installed
extension reports version 0.2.3; its `src/subagent/hosted-session.ts` is byte-for-byte
identical to the checkout's version. No business worktree or live child was changed.

## Symptom and corrected timeline

This is a supervision/state-tracking orphan, not evidence that an OS process lost
its parent or that the first interrupt failed to stop its turn.

Parent session: `01a1da73-7e75-7150-8603-1c7899f4610a`.
Child: `ch_2386fec0`; run: `run_a28d018c`.

All times below are UTC on 2026-11-15.

| Time | Evidence |
| --- | --- |
| 16:59:06.547 | Parent calls `subagent interrupt`. |
| 16:59:06.560 | Actual child session records assistant `stopReason=aborted`, error `This operation was aborted`. |
| 17:01:06.717 | Daemon records `agent.park`, reason `retention`. |
| 17:03:51.173 | Parent calls `agent_message send` with a new task. Tool accepts a new turn. |
| 17:03:51.530 | Child session records new `session_info`, consistent with respawn. |
| 17:03:52.036 | Parent child record ends as `failed` with the previous abort error; actual child session records the new user prompt at the same timestamp. |
| 17:04:00.331 | Actual child emits a new bash tool call; many more follow. |
| 17:04:23.185 | Parent, trusting the false failed state, launches replacement `ch_9b03efdd` in the same worktree. |
| 17:08:54–17:09:43 | Old child's transcript records `write` calls for the new work. |
| 17:11:11.647 | Replacement reports concurrent changes and refuses to write. |
| 17:11:24.866 | Parent calls interrupt on the old child again. |
| 17:11:24.868 | Daemon records `agent.park`, reason `interrupt`. |

Sources (local, not copied into this repository):

- Parent JSONL supplied by the user, lines 515–547.
- Child session JSONL:
  `~/.pi/agent/sessions/--Users-leechael-workshop-phala-teehouse--/2026-11-14T15-49-55-569Z_ch_2386fec0.jsonl`,
  notably lines 1068–1072.
- `~/.pi/agent/pi-famulus/sessions/<parent>/events.jsonl`.
- `~/.pi/agent/pi-famulus/sessions/<parent>/agents/ch_2386fec0.{json,jsonl}`.
- `~/.pi/agent/pi-famulus/agents/ch_2386fec0.json` (hosting metadata; environment/token fields not reproduced).

The peer session confirmed the corrected ordering. Sending to an idle/failed child
is intentionally supported; accepting that send is not itself the bug. Likewise,
a reused run changing from completed to pending is not by itself evidence of a bug.

## Root cause

1. `HostedRpcSession.prompt()` (`extension/src/subagent/hosted-session.ts:133`)
   installs a completion waiter **before** calling `agentInput`.
2. `HostedRegistry::rpc()` (`manager/src/hosted.rs:380`) first calls `ensure_warm`
   for a parked child, then sends the actual prompt.
3. The cold-start `get_state` handshake reports `isStreaming=false`.
   `ensure_warm` publishes `activity=idle` (`hosted.rs:354–364`) before the new
   prompt has run.
4. `HostedRpcSession.onManagerEvent()` (`hosted-session.ts:216–223`) resolves
   the pending completion waiter on **any** idle state. There is no turn-start
   fence distinguishing startup-idle from completion of this prompt.
5. If the prompt continuation observes idle before the new `agent_start`,
   `prompt()` and `waitForIdle()` complete prematurely. The runner's
   `finishGeneration()` (`runner.ts:726–758`) reads `lastFailure`, which still
   contains the previous turn's abort error. It reports the new turn as failed.
6. `settle()` (`runner.ts:790–822`) clears watchdog/deadline timers, releases
   admission capacity, and resolves the result/wake. The actual RPC prompt still
   proceeds in the child. Later tool events do not reactivate that settled
   runner generation (`runner.ts:939–980`).

The old failure explains the **failed** label, but is not the primary cause:
without an old failure this same race reports **completed** while work continues.
Clearing `lastFailure` alone would turn a false failure into a false success.

The reproduction does not require a late callback from the interrupted generation:
that turn is fully drained before resume. The startup-idle event alone is enough.

## Reproduction evidence

Both diagnostics below started as RED regression reproductions on `main`; they are
now clean, named regression tests. The production extension, registry, runner and
`HostedRpcSession` run in them; only the daemon transport is substituted.

### Fast deterministic transport replay

File: `extension/tests/unit/impl-hosted-resume-supervision.test.ts`.

Entry is the actual `agent_message send` tool. It replays an abort message and the
parked-child startup-idle event before the new agent starts. The invariant is that
executing resumed work must not have a terminal wake.

```sh
cd extension
./node_modules/.bin/vitest run tests/unit/impl-hosted-resume-supervision.test.ts
```

Before the fix this reproduced the incident twice with the same verdict:

- warm control: PASS;
- parked resume: FAIL, premature `subagent-done`, `status=failed`,
  `error="This operation was aborted"`;
- test body about 34–37 ms, whole invocation about 1.4–1.8 s.

With the fix the file has 13 passing cases: the two incident replays (warm
control and parked resume, including child-record status while the resumed child
executes tools), the stale-abort-error false-failure variant, the
stale-`agent_settled` fence, a startup idle delivered **before** and one delivered
**after** the prompt ack, `queued` dispositions settled after and before their
ack, fast completion before the prompt RPC response, a `handled` prompt that runs
no work after a finished turn, a process failure reported after the ack and one
reported before it (held on the turn until accepted), a rejected dispatch RPC,
and a prompt steered into a live out-of-band run

### Real daemon and real RPC Pi

File: `extension/tests/integration/hosted-resume-supervision.test.ts`.

Uses an isolated temporary home/workspace and the existing local `faux` model
fixture (no provider requests or business files). Starts a turn, interrupts,
parks explicitly instead of waiting for retention, then resumes via
`agent_message send`. The fixture records that the second model request is
executing and deliberately remains pending.

```sh
cd extension
PI_FAMULUS_INTEG=1 \
PI_FAMULUS_MANAGER_PATH=/Users/leechael/.pi/agent/npm/node_modules/pi-famulus-darwin-arm64/bin/pi-famulus \
./node_modules/.bin/vitest run tests/integration/hosted-resume-supervision.test.ts
```

Observed before the fix, about 18 seconds:

```text
RESUMED_CHILD_EVIDENCE {"modelCalls":2,"status":"completed","attempts":2}
AssertionError: expected 'completed' to be 'running'
```

The local faux abort did not leave the same assistant failure as the real
provider, so this demonstrates the false-success variant rather than duplicating
the error string. The isolated daemon is shut down by the test cleanup.

After the fix the same command passes: the parent record stays `running` with the
second model request executing and no new terminal wake is written.

## Fix (implemented)

`HostedRpcSession.prompt()` no longer resolves on "the daemon is idle". Each
prompt owns an `ActiveTurn` whose completion is fenced by that turn's own
lifecycle:

- Completion is fenced by the turn lifecycle, not by a generic idle snapshot:
  the turn and its promise are still created before `agentInput`, but they cannot
  resolve until the accepted prompt's own `agent_start`/`agent_settled` sequence
  ends. Acceptance and disposition are applied after the RPC response.
- `agent_state idle` only records transport/process readiness. A respawned
  parked child's cold `get_state` handshake can therefore say idle before the
  resumed prompt runs without ending the turn. Completion requires
  `agent_settled`, which pi defines as "no remaining automatic work (retries,
  compaction recovery, queued messages)".
- Prompt dispositions are handled explicitly: `started` waits for an
  `agent_start` after the turn was armed plus its settle; `queued` waits for the
  single settle of the run it joined, whether that settle arrives before or after
  the ack; `handled` resolves immediately because no run starts for that prompt.
  A missing disposition (older transport) behaves like `started`. The queued
  cases without a known active run are defensive transport tests: today's plain
  `prompt` rejects while streaming; `prompt_steer` is the real joined-run path.
- A prompt dispatched while a run is already in flight (the runner is idle but a
  user attach or a sibling child drives the hosted child) is sent as
  `prompt_steer` and remains supervised: it completes on that run's
  `agent_settled`, not when the steer is acknowledged. A `handled` steer still
  waits for the live run, because the child is doing work the parent must track.
- An `agent_settled` observed before any `agent_start` of this turn (a late
  record from the drained previous generation) is not treated as completion, and
  a later `agent_start` invalidates an earlier settle so Pi's internal retries
  stay supervised.
- Failure state is turn-scoped: the previous turn's abort error is cleared when
  a turn is armed, and assistant `message_end`/usage/transcript records from a
  drained generation are ignored (attributed by run-start ownership). A
  process-level `activity=failed` finishes the turn as failed even before the
  run starts; a failure that arrives before the RPC response is held on the turn
  and applied when the prompt is accepted, and a rejected dispatch still fails
  the turn with the RPC error.
- The result *text* is deliberately not turn-scoped: `getLastAssistantText()`
  stays the child's most recent assistant text so a stall retry keeps the text
  produced before the stall. An empty or `handled` new turn therefore reports the
  previous turn's text; only status and error are turn-scoped.
- `abort()` and `dispose()` resolve the outstanding turn (in `finally` for a
  failing abort), so supervision and the runner's prompt promise cannot leak.

No protocol, version or configuration change was needed: the daemon already
forwards `agent_start`/`agent_settled` and the prompt disposition. Runner and
registry code are unchanged; the runner's existing `finishGeneration()` now runs
only when the real turn ended.

### Verification commands and results

```sh
cd extension
./node_modules/.bin/vitest run tests/unit/impl-hosted-resume-supervision.test.ts
# 13 passed (with the fix reverted: 6 failed / 7 passed — parked replay, stale
# settle fence, idle after ack, handled-without-run, process failure after and
# before the ack; the warm control, stale-error, queued, fast-completion,
# steered-run and dispatch-rejection cases behaved the same on the old code)

./node_modules/.bin/vitest run tests/unit/impl-hosted-resume-supervision.test.ts \
  tests/unit/impl-subagent-runner.test.ts \
  tests/unit/impl-subagent-lifecycle.test.ts \
  tests/unit/impl-hosted-child-resume-by-id.test.ts
# 4 files, 84 passed

npm run test:compat
# 16 files, 93 passed

/Users/leechael/.vite-plus/js_runtime/node/24.19.0/bin/node \
  node_modules/vitest/vitest.mjs run tests/unit
# Node 24.19.0: 100 files passed, 1308 passed, 2 skipped.
# On this shell's default Node 26.13.1: 1307 passed, 2 skipped, 1 failed:
# impl-bounded-output.test.ts:73 (src.isPaused() after stream end).
# The same isolated test passes on Node 24 and fails on Node 26, including
# with this fix reverted. Its source and test are unchanged by this branch.

./node_modules/.bin/tsc --noEmit
# no output

PI_FAMULUS_INTEG=1 \
PI_FAMULUS_MANAGER_PATH=/Users/leechael/.pi/agent/npm/node_modules/pi-famulus-darwin-arm64/bin/pi-famulus \
./node_modules/.bin/vitest run tests/integration/hosted-resume-supervision.test.ts
# 1 passed, about 17s
```

Remaining risks:

- Without a protocol-level turn id, attribution is inferred from event order. A
  stale `agent_start` delivered after a new turn is armed (rather than before,
  which the interrupt/abort drain already guarantees) could still be counted for
  the new turn.
- The real-daemon test proves the false-success variant. The false-failure
  (previous abort error) variant is proven by the transport replay, because the
  local faux abort does not emit a provider-style failure message.
- `prompt()` still decides between `prompt` and `prompt_steer` from its own
  streaming view. The steered path is now supervised, but a child that is
  actually streaming while the adapter believes it is idle still surfaces pi's
  rejection as a failed turn instead of waiting for the run. That pre-existing
  detection gap is not addressed here; closing it needs a state/pid fence or a
  protocol change.
- Reconnect while a turn is outstanding, simultaneous parent inputs and
  automatic-recovery ordering were not exercised; the new unit cases cover
  dispositions, startup idle before/after the response, stale settle, process
  failure and dispatch rejection only.
- An empty or `handled` new turn can report the previous turn's assistant text
  in its result (status/error are correct); scoping the text would need the
  runner to distinguish a user resume from a stall retry, which this change
  deliberately does not do.

## Operational implications

On a build without this fix, do not assume a terminal parent record proves a
resumed parked child is idle. Check hosted state/PID/session activity before
assigning the same worktree to another agent. A fresh child avoids this reuse
path only if the previous child is first confirmed stopped. With the fix
installed, a resumed hosted turn stays `running` until the child's own
`agent_settled`; the installed 0.2.3 build predates it.

Hosted children are visible as `__rpc ... --session-id ch_* --name ...` processes.
Grepping only for the business worktree or `__run` misses their read/edit/write
activity; it does not prove that no child process exists.
