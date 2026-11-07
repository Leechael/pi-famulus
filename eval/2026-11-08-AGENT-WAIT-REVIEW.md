# 2026-11-08: how the agent waits (default-persistent monitor)

**Status:** a monitor started with neither `timeout_ms` nor `persistent` is now persistent, and the monitor prompt says so. In a clean context this moved readiness waiting from a persistent monitor in 0/3 episodes to 3/3 of the monitors started without a timeout. In a replay of the incident's own context it did not change the choice. The [BASELINES.md](BASELINES.md#when-to-rerun) gate is **still open**: this changes a tool description, and the default scenarios have not been rerun. All results are deepseek-flash only.

## Incident

The 2026-11-07 log-monitor session (`deepseek-flash:high`, pi-famulus 0.2.3):

- 76 bash `sleep` calls of 40-70 s.
- 258 `task_output` calls, 326 of them "no new output" replies.
- Producers started with `(... &)` and then checked with `ps`.
- 10 of 17 monitors were started with an explicit `timeout_ms` (600000 to 2400000).
- In its thinking, 65 of the 70 waits of `sleep` 10 s or more never mentioned `monitor`.

The monitor prompt (0.2.3, and 0.2.4 in other words) presented `timeout_ms` as the lifetime for a watch expected to end within an hour and `persistent` as the opt-in for longer ones, so the model estimated a duration itself. None of the 10 timed monitors relied on the 5-minute default.

## Changes

- **Code** (`extension/src/monitor.ts`): neither `timeout_ms` nor `persistent` means persistent (runs until its command exits or `task_stop`). `persistent:false` keeps the 5-minute default. `persistent:true` with `timeout_ms` is still rejected.
- **Prompt** (`extension/prompts/tools/monitor.md`): a monitor runs until its command exits or you stop it with `task_stop`; `timeout_ms` is only for a time-bounded watch the user asked for. The lifetime is stated once in the description; the rule line and the parameter descriptions are short and exclusive.
- **Eval**: grading lifetimes from the monitor start result, two opt-in probes, and replay tooling (below).

## Eval tooling

- **Lifetime from the start result.** The `monitor-behavior-*` graders read the effective lifetime from the start result (`details.timeout_ms`, `null` = persistent), not from the call arguments. The opt-in count goes from 15 to 17.
- **Probes** `agent-wait-job` and `agent-wait-ready` (`ablation/agent-wait-scenarios.ts`) put the agent at the incident's kind of decision point: it starts a job or a server and has to wait, and the request never says "watch" or "monitor". See the [README](README.md#opt-in-monitor-behavior-probes).
- **Decision-point replay** (`replay/run-replay.ts`, `replay/wait-choice.ts`). 10 decision points of the incident session, k=5 per arm, `deepseek-flash:high`. Only turn 0 is graded, because replayed bash returns fake output. PASS is a persistent monitor, or ending the turn when something already runs. About 20 of the 50 episodes per arm made no wait decision at turn 0.
- **Prompt splice** (`replay/prompt-splice.ts`). A replay reuses the session's own system prompt, so it shows the recorded 0.2.3 rules while the tool definitions come from the extension under test. The splice swaps the recorded pi-famulus texts for the extension under test's.

## Replay of the incident context

| Arm | PASS / FAIL |
|---|---:|
| main | 6/24 |
| "prefer persistent" wording | 5/24 |
| `wait_until`, an alias of `monitor` | 2/28 |
| default-persistent + new prompt | 5/29 |

The alias was called twice in 100 episodes and was dropped. Timed monitors in the replay reused timeout values from the session's own history (1500000, 1800000, 2400000, 600000, 900000).

In a context already full of the bad pattern, neither wording, naming nor the default changes the choice. The arms are within what k=5 can separate. Guard arms of the same replay are in the Wait guard section.

## Clean-context probes

`deepseek/deepseek-flash:medium` (the provider reported thinkingLevel high), k=3. Arm A is main 0.2.4; arm B is default-persistent + the new prompt.

| Probe | A | B |
|---|---:|---:|
| `agent-wait-job` | 3/3 | 3/3 |
| `agent-wait-ready` | 0/3 | 2/3 |

- `agent-wait-job`: all six passed through the bash completion wake.
- `agent-wait-ready`: readiness came from a persistent monitor in A 0/3 and B 3/3. A passed explicit timeouts every time (120000, 90000, 120000). B started all three without a timeout. B's one failure was `task_list`/`task_output` polling while the monitor ran.

First run of the `monitor-behavior-*` probes (the agent-wait rows of that run are superseded by the table above):

| Probe | A | B |
|---|---:|---:|
| long | 2/3 | 3/3 |
| hour | 3/3 | 3/3 |
| natural | 2/3 | 2/3 |
| renew | 2/3 (+1 INVALID) | 1/3 |
| noise | 2/3 | 3/3 |
| cleanup | 1/3 | 3/3 |
| alternative | 1/3 | 1/3 |

The hour control (an explicit one-hour watch must keep `timeout_ms: 3600000`) shows no regression under the default.

## Wait guard

The prompt and the default did not change the choice in the incident context, so a mechanism does. `extension/src/wait-guard.ts` runs before a bash call and blocks it without ending the run when it waits in bash (`sleep N` with N of 10 or more, or a `sleep` inside a `while`/`until`/`for` loop body) or detaches a process (a lone `&`, `nohup`, `setsid`, `disown`). Quoted strings, heredoc bodies and comments are ignored. The refusal names the alternative: a persistent monitor, ending the turn, or running in the foreground. Config `waitGuard`, default true; `false` disables it (`mech.wait-guard` in the ablation manifest).

Replay of the incident context (same 10 decision points, k=5 per arm, turn 0 only), PASS / FAIL:

| Arm | PASS / FAIL |
|---|---:|
| main | 6/24 |
| wait guard | 9/24 |
| `wait_until` alias + guard | 9/22 |
| default-persistent + new prompt | 5/29 |
| default-persistent + new prompt + guard | 9/25 |

With the guard, sleep plus detach choices went from 16 to 0, and polls from 3 to 12: the model leaves bash waiting and moves to `task_output`/`task_list`.

Clean-context probes (same model, k=3). Arm C is arm B (default-persistent + new prompt) plus the wait guard.

| Probe | A (main 0.2.4) | B | C |
|---|---:|---:|---:|
| `agent-wait-job` | 3/3 | 3/3 | 3/3 |
| `agent-wait-ready` | 0/3 | 2/3 | 1/3 |
| long | 2/3 | 3/3 | 3/3 |
| hour | 3/3 | 3/3 | 2/3 |
| natural | 2/3 | 2/3 | 2/3 |
| renew | 2/3 (+1 INVALID) | 1/3 | 2/3 |
| noise | 2/3 | 3/3 | 2/3 |
| cleanup | 1/3 | 3/3 | 1/3 |
| alternative | 1/3 | 1/3 | 2/3 |

- `agent-wait-job`: all nine passed through the bash completion wake.
- `agent-wait-ready`: readiness came from a persistent monitor in C 3/3. C's two failures: one model overrode the fixture env (`READY_MS=100 SERVER_TOKEN=...`), so the token did not match the smoke output; another, after the guard refused a `for`-loop wait, read the task output file.
- The eval found that the guard missed `for ... do ... sleep ... done`. That is fixed, and a `sleep` after a loop's `done` is not flagged.
- The guard did not help `agent-wait-ready` in a clean context (B 2/3, C 1/3); in the incident-context replay it raised PASS from 6/24 to 9/24.

**Deferred:** the poll guard, which would end a run on `task_output` of a still-running task. A replay count showed 43 trips. `deferred: poll guard (end a run on task_output of a still-running task) | impact: with the wait guard on, waits move to task_output/task_list polling (polls 3 to 12 in replay) | trigger: polling remains the dominant wait pattern in a real session after the wait guard`

## Harness and grader issues found and fixed during the run

- The ablation runner's "floor" early stop skips variant cells when the baseline arm is below 20%. Arm B's agent-wait cells were therefore run separately, as a baseline against an extension copy.
- Transcripts of two runs with the same variant name overwrote each other in one directory. The agent-wait probes were rerun per arm in separate result directories.
- The `agent-wait-ready` grader first required the server itself as the monitor command. It was relaxed to "readiness from any persistent monitor's wake".
- Polls and sleeps are counted only in the waiting window. A cleanup `task_list` after the result is not a poll.
- Arms A and B were regraded from saved transcripts with the final grader.

## Limits

- One model, k=3 per cell: single-cell differences are noise.
- The fixtures expose the `READY_MS` and `SERVER_TOKEN` env names in source. One episode gamed it; the grader requires the token from the smoke output, so gaming fails rather than passes.
- The replay grades turn 0 only, and its tool results are fake.
- `deferred: rerun the default scenarios per BASELINES.md | impact: an unmeasured regression elsewhere from the changed monitor description is possible | trigger: before the next release`
