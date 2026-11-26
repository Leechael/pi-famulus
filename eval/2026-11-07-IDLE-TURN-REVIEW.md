# 2026-11-07 idle turn guard review

## Incident

Two sessions delegated over cross-session messages, with no user turn, could not end their turn:

- Incident A (`kimi-coding/k3.1-pro:high`): after its final report it ran 345 more turns in 31 minutes, each "完成。" plus `task_list`, `date`, `true`, `printf ''`, or a repeated read-only check (`git status`, `gh pr view`), until the user aborted. About 40M input tokens. Its thinking said "Stop." on nearly every turn.
- Incident B (`kimi-coding/k3-256k:high`): waiting on a CI monitor, it alternated `true` and `task_list` for 8 minutes until the monitor's wake arrived.

The prompt already says to reply with no tool call when no work remains. Nothing enforced it. Extension tree `18a8291` adds the idle turn guard (`extension/src/idle-turn-guard.ts`): it ends the run at 2 idle turns in a row or 3 in one run.

## Decision-point replay (real context, real model)

`replay/run-replay.ts` resumes the real session from an entry, with the session's own system prompt, and caps each episode at 12 assistant turns or 180 s. Tool calls never leave the sandbox: bash runs `true`, read reads an empty file, other tools are blocked. Session files and outputs are not committed.

Each start point was run k=5 per arm, with the guard off (`idleTurnGuard: false`) and on.

| start point | model | runaway, guard off | runaway, guard on | turns, guard on |
|---|---|---|---|---|
| A, right after the final report | k3.1-pro:high | 3/5 | 0/5 | 2, 6, 3, 1, 4 |
| A, 3 idle turns in | k3.1-pro:high | 5/5 | 0/5 | 4, 5, 3, 4, 5 |
| B, monitor just armed | k3-256k:high | 2/5 | 0/5 | 2, 1, 2, 1, 1 |

Runaway = 12 turns without a reply and without a guard trip. Totals: guard off 125 turns (a lower bound, because of the cap), 10/15 runaway; guard on 44 turns, 0/15 runaway, 11 trips. Every guarded trajectory stopped at the second idle turn in a row or the third in the run.

The first replay without the guard interleaved no-ops with fresh read-only checks (`task_list`, `gh pr view`, `pwd`, `read`, `echo done`). A consecutive-only count never reached 2, which is why the per-run limit was added.

## Synthetic probes do not reproduce it

The opt-in `idle-after-done-*` probes background a 25 s check and require a report. The peer variants deliver the task as a cross-session message with the real cross-session prompt and SendMessage surface. The `-long` variants prepend ~90k tokens of incident-shaped history, and the observed context was ~115k tokens. Five k3.1-pro:high episodes (peer, peer-long, user, with the guard on and off) all reported and ended their turn with 0 idle turns. A k=5 two-model grid was stopped after 4/4 such passes, because the replay was far more informative per episode. These probes stay as regression checks for the bounds, not as reproductions.

## Regression check: default turn-control probes, guard on

Baseline, k=3: `bg-end-turn`, `wake-continue`, `still-running-continue`, `monitor-not-sleep` on `kimi-coding/k3.1-pro:high` and `openai/gpt-6-luna:medium`. Results: 22 PASS, 2 FAIL, 1 ERR, with 0 guard trips in any transcript. The two failures are luna `monitor-not-sleep` "polled" (`task_output`/`task_list` once or twice after arming a monitor, below the guard's bounds). The ERR is a k3.1-pro provider response cut off at the episode cap.

## Limits

- The replay rewrites tool calls, so later results are "(no output)" instead of the real ones, and its tool set differs from the incident's. Both arms share these differences.
- Only kimi models were replayed. The false-trip check covers four probes and two models at k=3.
- The guard does not catch a loop made only of fresh read-only commands (e.g. `git status` and `gh pr view` alternating). deferred: repeat-call detection | impact: such a loop still runs until abort | trigger: an incident whose loop avoids every idle probe.
