# 2026-11-05: monitor lifetime (timeout vs persistent)

**Status:** the candidate improved the targeted monitor probes on 11 models. The [BASELINES.md](BASELINES.md#when-to-rerun) gate is **still open**: this changes a tool description, and all ten default scenarios have not been rerun.

## Incident

In a real session, an agent asked to watch something long-term chose `timeout_ms` = 1 hour and then re-armed the monitor by hand every hour, for 7–8 rounds. Its own explanation was that it had read only the timeout part of the `monitor` description and missed `persistent`.

## Probes

Opt-in probes in `ablation/monitor-waiter-scenarios.ts` (`behaviorProbe`), run on eleven `:medium` specs: `openai-codex/gpt-6-{astra,luna,sol}`, `openai-codex/gpt-6.1-{sol,terra}`, `deepseek/deepseek-flash`, `kimi-coding/{kimi-for-coding,k3-256k,k3.1-pro}` and `xai/grok-4.{6,7}`.

| Probe | User asks | Expected |
|---|---|---|
| `long` | ongoing observation, "no planned end" | one `persistent:true` monitor |
| `hour` | observation that lasts exactly one hour | `timeout_ms: 3600000`, not persistent (control against overuse) |
| `natural` (new) | "keep an eye on it for the rest of the day" | `persistent:true` (the horizon is longer than the 1-hour cap, with no explicit cue) |
| `renew` (new) | a ten-second trial monitor, then "the rest of the day" once it ends | a timed trial; after its real timeout, a persistent continuation rather than another timed monitor |

`natural` and `renew` were reviewed for confounds before running. The probe for the incident's exact shape, where the model's own timed monitor expires with no user cue, is deferred (see the [README](README.md#opt-in-monitor-behavior-probes)).

## Round 1 (old grader and fixture; not comparable)

142 episodes: 87 PASS, 33 FAIL, 19 INVALID, 3 ERR. No episode showed the incident's hourly re-arming: every continuation that reached the renew decision point chose `persistent`. Reviewing every FAIL and INVALID transcript turned up harness defects that make round 1 unusable as a baseline:

- **Fixture leaks.** The fixture source contained a comment describing the timeout and batching, and its code showed that ALERT fires within seconds. Models `cat` the file; grok-4.7 chose a 2-minute and a default timeout in `natural` after reading it.
- **Shared state between episodes.** Concurrent episodes shared `/tmp` and the process table. Models found other episodes' `/tmp/behavior-source.log` and processes and spent their turns investigating them.
- **Pre-start polling.** Reconnaissance before any monitor existed (`ps`, `pgrep`) was counted as polling.
- **Expiry ordering.** `renew` ordered expiry by sequence number, but terminal notices are batched (≤10 s) and arrive late.
- **Cutoff.** 60 s was too short for the kimi models.

Real findings from round 1:

- **Silent parameter handling.** `timeout_ms` together with `persistent:true` silently became persistent, so a "watch for exactly one hour" never ended. `timeout_ms` above the cap was silently clamped to 1 hour, so an 86,400,000 ms request (seen in round 2's baseline) ran for 1 hour.
- **Command shape.** Models ran the producer in background bash, then tailed its output file (16 episodes). The description's only example was `tail -F file`.
- **Lifetime inside the command.** Models chose `persistent:true` and then bounded the watch inside the command (`timeout 3600`, their own timer). Likely cause: "Silence is not success" with no statement that exit and timeout are reported anyway.
- **Undefined "long".** "Use persistent:true for long monitoring" never said what long means, and never tied it to the 1-hour cap.

## Changes

- **Prompt** (`extension/prompts/tools/monitor.md`):
  - Pick one lifetime: `timeout_ms` for a watch that ends at a known time within an hour; `persistent:true` when the watch is open-ended or longer than an hour ("rest of the day", "until I say"). Do not re-arm timed monitors, never set both, and never put the lifetime inside the command.
  - When you start the producer yourself, make it the command; use `tail -F` / `journalctl -f` only for sources you did not start. Add `-m1` only when the first match should end the watch.
  - Exit and timeout are reported, so do not add end-of-watch lines. Report from the `<event>` itself, without `task_output`/`task_list` or re-reading to confirm it.
  - The `timeout_ms` and `persistent` parameters are described as exclusive. The vague "long monitoring" rule and "Prefer tail -F over polling" are removed.
- **Code** (`extension/src/monitor.ts`): `timeout_ms` with `persistent:true`, or `timeout_ms` above 1 hour, is rejected with an error that points to `persistent:true`. No internal path restarts monitors from stored parameters (enumerated during the change).
- **Harness**:
  - The fixture's timing comes from the environment, and the source carries no comments.
  - Each episode gets its own `TMPDIR`. Polls are counted only after the first monitor start, and `renew` orders expiry by wall-clock time.
  - Each scenario can set its own idle give-up, and probe episodes are capped at 120 s. The `renew` prompt names the monitor tool for the trial.
  - Opt-in-only matrix sections no longer list unrun default scenarios.
  - Every grader change has a deterministic test that fails on the old behavior.

## Round 2: A/B

Both arms ran the same harness, grader, fixture, models, k=3 and concurrency 2, in parallel. The arms differ only in the extension: baseline is tree `95ffe54` (commit `c12f2f7`); the candidate is tree `bb71504`. Matrices: [RESULTS.md](RESULTS.md) 2026-11-05b and 2026-11-05c.

| Graded episodes (PASS/FAIL only) | Baseline | Candidate |
|---|---:|---:|
| Episodes | 131 | 132 |
| PASS | 92 (70%) | 116 (87%) |
| Wrong lifetime | **10** | **0** |
| Command not running the producer | 26 | 2 |
| Polled after the start | 13 | 10 |

The 10 baseline lifetime errors:

- **Too short for an open-ended watch (the incident class):** grok-4.7 `natural` chose `timeout:300000` and `timeout:120000` even with the timing hidden, and luna `renew` chose `timeout:86400000`, which the old code silently clamped to 1 hour.
- **Persistent where one hour was asked:** sol ×2, terra and deepseek in `hour`, which never ends, or ends only through a timer inside the command.
- **Default or missing timeout:** luna `long` and `hour`, kimi-for-coding `hour`.

The candidate has none of these.

Per model, the candidate is equal or better on every probe except one cell: kimi-for-coding `long`, 3/3 → 2/3. That one failure is polling, which kimi also does in its baseline cells, and is within noise at k=3.

Remaining candidate failures, read from the transcripts, are unrelated to lifetime choice:

- **Polling (10 episodes):** mostly kimi and k3 in `renew`. The new "report from the event" wording barely moved it.
- **Duplicate starts:** luna (4), deepseek (1). They start a monitor, then restart it with a different filter. This also appears in luna's baseline (7 of its 15 episodes started more than once).
- **Command shape:** luna (2).

## Limitations

- k=3 per cell, so single-cell differences are noise. The aggregate lifetime-error difference (10 vs 0 of about 130) is the claim.
- Probes check the selected parameters and the first alert, not hours of operation, cost or wake rates.
- On macOS, models can still see the host's process table, including the operator's own processes. Concurrency was lowered, not eliminated.
- `deferred: synthetic-history expiry probe (the model's own timed monitor expires with no user cue) | impact: the incident's exact decision point is untested; renew and natural cover it only with a user cue | trigger: hourly re-arming recurs in a real session after this change`
- `deferred: rerun all ten default scenarios per BASELINES.md | impact: an unmeasured regression elsewhere from the longer monitor description is possible | trigger: before merging this change`
