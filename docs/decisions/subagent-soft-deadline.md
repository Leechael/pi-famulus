# Subagent soft deadline

**Status:** Accepted. Implemented in `feat/subagent-soft-timeout`.
**Decision:** A subagent reaching its time budget (`timeout_ms` / `subagent.timeoutMs`) is not aborted. The parent gets a `<pi-famulus-wake kind="subagent-overrun">` and decides: `extend`, `steer`, or `interrupt`. If the parent does nothing the child keeps running and the reminder repeats every `overrunRepeatMs` (default 10 min) until it settles. A hard ceiling that aborts, `hardTimeoutMs`, is opt-in and off by default.
**User decision:** "超时应该是先给主进程发消息，不要 kill，这个太伤了" (on timeout, message the main process first; don't kill, that hurts too much). Not re-asked.

## Inciting run (2026-10-05)

A parent ran worker and reviewer subagents with the default 30-minute `timeoutMs` on a CPU-oversubscribed host. Counts below come from the session's `events.jsonl`, recounted by script for the PR:

- 23 `agent.timeout` events across 9 children; every one settled `interrupted (timeout)`.
- Every timeout landed 30.0 min after that child's latest `agent.start`. The budget restarts on each `resume`, so children resumed together timed out together.
- 21 `task.stop reason=tool` events, each within 2 s of an `agent.timeout`: the abort cancelled the child's foreground shell. Those shells had run 251.1 min in total at the moment they were stopped, 10 of them for 10 min or more (test suites).
- The 9 timed-out children's transcripts hold 21 `Command aborted (task stopped)` tool results, one per stopped shell. The runner sent the child nothing naming the budget (it settled and aborted). The parent could not lengthen a running child's budget (`timeout_ms` was fixed at spawn and `resume` ignored it).

## Why a soft deadline

The budget exists so a parent notices a child that runs long. The abort answered that question for the parent, and answered it badly: the expensive work in flight (a long test run) is exactly what a time budget hits first, and aborting it throws the work away. The parent has context the runner lacks (is this test suite expected to take 20 min? is the child looping?), so the runner reports and the parent decides.

## Alternatives considered

- **Keep the hard abort, raise the default.** Moves the cliff; the work lost at the cliff is still the longest-running work.
- **Abort the turn but let the running shell finish.** The child loses its turn mid-tool, and the shell result has no reader.
- **Extend automatically while a shell's output is growing.** The runner would be guessing the parent's intent; CPU accounting (separate PR) would make the guess better, but the decision still belongs to the parent. The wake carries the shell's output size, idle time and a growing flag so the parent can judge.
- **Retry timeouts like stalls.** Rejected in the stall ADR and still rejected: a timeout is a budget, not a fault.

## Semantics worth pinning

The transition table is in `docs/design.md` §4.6 ("Child lifecycle"). Points that change earlier contracts:

- **Stall interplay (supersedes S1 of `stall-auto-resume.md` for the soft deadline).** The stall watchdog is unchanged: no session events for `stallMs` outside tool execution → abort and auto-resume. The soft deadline is a turn-level schedule, re-armed by each stall retry from the turn's `softDeadlineAt`/`nextReminderAt`; a retry never restarts the budget. A soft deadline that lands during a retry delay sends the overrun wake (the child is still running from the parent's view) and the retry still happens. The old S1 behaviour ("a timeout landing during a retry delay still fires", "stall retries arm only the remaining time") now applies only to `hardTimeoutMs`.
- **Repeat.** One wake per `overrunRepeatMs` while the child runs. The reminder count continues across `extend` within a turn and resets on `resume`.
- **"The parent acts".** `extend` re-arms the deadline at `now + timeout_ms` (default: the child's spawn budget, kept by owner decision on PR #34). `steer`/`followUp` (including `agent_message` send/queue/broadcast) postpone the next reminder by `overrunRepeatMs` once a reminder has fired and the current (possibly extended) deadline has passed; they never move the deadline, and a delivery that lands after its turn ended changes nothing (review findings on PR #34). `interrupt` settles. Nothing else counts.
- **Pending `need_decision`.** Reminders are held while the child waits on the parent: the parent already has a `supervisor-request` wake for it, and an overrun wake on top would ask it to decide about a child that is blocked on it. The budget is not reset. When the decision resolves, a reminder that came due is sent at once. The hard ceiling is not held. (Owner decision on PR #34: either hold or name the decision; holding was picked because it adds no wake text.)
- **Hard ceiling.** `hardTimeoutMs` is measured from the same `turnBudgetStart` and is not moved by `extend`: it is the user's ceiling, not the model's. When it fires the child settles `interrupted (timeout)` and is aborted as before, including its foreground shell.
- **resume honours `timeout_ms`.** The new turn's budget is the `timeout_ms` passed to `resume`, else the spawn budget.
- **Timers.** Every new timer goes through the injected `Clock` and the generation `TimerScope`; settle, interrupt, resume and dispose clear them, and the callbacks re-check `settled/disposed/generation`.

## Defaults

- `overrunRepeatMs` 600000 (10 min). Shorter than the test suites in the inciting run (10–15 min is common there), so a parent hears again during one long suite rather than once an hour; long enough that a parent that chose to wait is not woken every few minutes. The other timers are staggered (stall 5 min, decision 10 min, budget 30 min); reminders land at 30, 40, 50 min.
- `hardTimeoutMs` 0 (off). The user decided not to kill on budget.

## Deferrals

- deferred: batching overrun wakes of children that cross their deadline together | impact: N children resumed together (the inciting run's shape) produce N separate overrun wakes within a second; the parent takes N turns, or one turn with N steered messages | trigger: an eval or transcript shows a parent mishandling simultaneous overrun wakes, or more than 3 in one minute in real use.
- deferred: CPU accounting in the overrun wake | impact: the parent judges progress from output size/idle time only; a quiet but busy process looks idle | trigger: the per-task CPU accounting PR lands (add its numbers to the `<shell>` element).
- deferred: eval scenario and ablation segments for the overrun wording (wake, tool text, guideline bullet), including simultaneous deadlines: several children crossing their budget in the same second, as resumed children did in the inciting run | impact: the wording is unevaluated; a model may poll, interrupt by reflex, or mishandle a burst of overrun wakes | trigger: before the next prompt-eval run, or the first transcript where a parent mishandles an overrun wake
- deferred: telling the child about the budget | impact: the child cannot pace itself; it learns of an overrun only if the parent steers it | trigger: transcripts show parents steering overrun children with budget information repeatedly.
