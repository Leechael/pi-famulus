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

In a context already full of the bad pattern, neither wording, naming nor the default changes the choice. The arms are within what k=5 can separate. Guard arms of the same replay are in the wait guard PR.

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

## Other models: gpt-6-luna and gpt-6.1-terra

`openai-codex/gpt-6-luna:medium` and `openai-codex/gpt-6.1-terra:medium`, k=3, each arm run as its own baseline in its own results directory. `noise` is regraded from the saved transcripts with the report fix below; every other cell is the run's own verdict and matches the regrade.

| Probe | luna A | luna B | terra A | terra B |
|---|---:|---:|---:|---:|
| `agent-wait-job` | 3/3 | 3/3 | 3/3 | 3/3 |
| `agent-wait-ready` | 0/3 | 0/3 | 0/3 | 2/3 |
| long | 3/3 | 3/3 | 3/3 | 3/3 |
| hour | 2/3 | 2/3 | 3/3 | 3/3 |
| natural | 2/3 | 0/3 | 3/3 | 3/3 |
| renew | 1/3 (+1 INVALID) | 0/3 (+2 INVALID) | 3/3 | 3/3 |
| noise | 3/3 | 3/3 | 3/3 | 3/3 |
| cleanup | 0/3 | 0/3 | 3/3 | 3/3 |
| alternative | 0/3 (+1 INVALID) | 1/3 | 3/3 | 3/3 |

- terra behaves like deepseek-flash on `agent-wait-ready`: under main it sized the timeout itself (300000, 60000, 30000), under B it started the readiness monitor without one. B's failure is the next point.
- luna already chooses lifetimes correctly in every arm (`correctDuration` true in nearly every episode), so the default does not change it. All 6 of its `agent-wait-ready` episodes, and one terra B episode, failed another way: the server was started with `run_in_background` and the turn ended with nothing watching ("I'll run the smoke test once it's up"). A background command wakes the agent only when it exits, and a server never exits. That is addressed separately, not by this change.
- luna's other failures are command shape (the producer started in bash, then its output file tailed) and polls while a monitor runs, in every arm.
- No hour-control regression on either model.

## Harness and grader issues found and fixed during the run

- The ablation runner's "floor" early stop skips variant cells when the baseline arm is below 20%. Arm B's agent-wait cells were therefore run separately, as a baseline against an extension copy.
- Transcripts of two runs with the same variant name overwrote each other in one directory. The agent-wait probes were rerun per arm in separate result directories.
- The `agent-wait-ready` grader first required the server itself as the monitor command. It was relaxed to "readiness from any persistent monitor's wake".
- Polls and sleeps are counted only in the waiting window. A cleanup `task_list` after the result is not a poll.
- Arms A and B were regraded from saved transcripts with the final grader.
- The `monitor-behavior-*` report check required the literal `ALERT fixture-ready`. Models reformat it (`ALERT: \`fixture-ready\``): 8 `noise` episodes (6 luna, 2 deepseek-flash) were FAIL for formatting alone. The check now looks for the alert's unique payload, `fixture-ready`, after the alert wake. The deepseek-flash `monitor-behavior-*` cells above were scored before this fix; their arm A and C transcripts overwrote each other, so they cannot be regraded, and `noise` there may be understated by up to 2.

## Limits

- One model, k=3 per cell: single-cell differences are noise.
- The fixtures expose the `READY_MS` and `SERVER_TOKEN` env names in source. One episode gamed it; the grader requires the token from the smoke output, so gaming fails rather than passes.
- The replay grades turn 0 only, and its tool results are fake.
- `deferred: rerun the default scenarios per BASELINES.md | impact: an unmeasured regression elsewhere from the changed monitor description is possible | trigger: before the next release`
