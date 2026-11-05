# Eval results

One section per baseline-only run (the full prompt, no ablation variants), newest first. Rows are scenarios (their prompt and what they test: [`scenarios/`](scenarios/)); columns are model specs as `pi --model` takes them (`id:thinking`), with the provider prefix dropped when the short name is unambiguous. The 2026-10-08 matrix ran before [PR #62](https://github.com/Leechael/pi-famulus/pull/62) changed the `subagent` and wake prompts. Every cell is the grader's verdict, with no hand corrections: when a verdict is wrong, the grader or the scenario is fixed and the cell is rerun.

Generate a section from the run's result files (later files replace earlier ones cell by cell, e.g. a rerun of one scenario):

```bash
node ablation/matrix.ts --results results/<run>/a.jsonl,results/<run>/b.jsonl \
  --title "<date>: <what changed>" --note "<extension tree, harness commit, notes>" --write RESULTS.md
```

When to rerun and with which models: [BASELINES.md](BASELINES.md#when-to-rerun).

<!-- runs: newest first -->

## 2026-11-05c: monitor lifetime probes, candidate prompt + parameter validation

Extension tree `bb71504`: rewritten monitor lifetime, command and wake wording; `timeout_ms` with `persistent`, or above 1 hour, is rejected. Same harness, graders, fixture, models, k and concurrency as 2026-11-05b, run in parallel. 136 episodes: 116 PASS, 17 FAIL, 3 INVALID/ERR; no graded episode chose the wrong lifetime. Remaining failures are polling (kimi/k3 renew) and luna restarting monitors. See [2026-11-05 monitor lifetime review](2026-11-05-MONITOR-LIFETIME-REVIEW.md).

Blank: 3 of 3 scored episodes passed. X: any FAIL, or fewer than 3 scored (INVALID and errors excluded).

| scenario | tests | `gpt-6-astra:medium` | `gpt-6-luna:medium` | `gpt-6-sol:medium` | `gpt-6.1-sol:medium` | `gpt-6.1-terra:medium` | `deepseek-flash:medium` | `kimi-for-coding:medium` | `k3-256k:medium` | `k3.1-pro:medium` | `grok-4.6:medium` | `grok-4.7:medium` |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `monitor-behavior-long` | open-ended observation ("no planned end") selects a persistent monitor and reports its alert |  | X |  |  |  |  | X |  |  |  |  |
| `monitor-behavior-hour` | an explicit one-hour observation selects timeout_ms 3600000, not persistence |  | X |  |  |  |  |  |  |  |  |  |
| `monitor-behavior-natural` | an open-ended watch phrased naturally (no "no planned end" cue, horizon longer than the 1-hour timeout cap) selects persistent:true rather than a timed monitor |  | X |  |  |  | X |  |  |  |  |  |
| `monitor-behavior-renew` | after a short timed monitor expires, continuing an open-ended watch (horizon beyond the 1-hour timeout cap) starts a persistent monitor rather than another timed one |  | X |  |  |  | X | X | X | X |  |  |

<sub>results: `results/monitor-r2-candidate/results.jsonl`</sub>

## 2026-11-05b: monitor lifetime probes, baseline prompt (c12f2f7)

Extension tree `c12f2f7` (monitor prompt before the lifetime rewrite); harness, graders and fixture from the eval/monitor-persistent branch. Eleven `:medium` models, k=3, concurrency 2, run in parallel with 2026-11-05c. Opt-in monitor-behavior probes only. 135 episodes: 92 PASS, 39 FAIL, 4 INVALID/ERR; 10 graded episodes chose the wrong monitor lifetime. See [2026-11-05 monitor lifetime review](2026-11-05-MONITOR-LIFETIME-REVIEW.md).

Blank: 3 of 3 scored episodes passed. X: any FAIL, or fewer than 3 scored (INVALID and errors excluded).

| scenario | tests | `gpt-6-astra:medium` | `gpt-6-luna:medium` | `gpt-6-sol:medium` | `gpt-6.1-sol:medium` | `gpt-6.1-terra:medium` | `deepseek-flash:medium` | `kimi-for-coding:medium` | `k3-256k:medium` | `k3.1-pro:medium` | `grok-4.6:medium` | `grok-4.7:medium` |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `monitor-behavior-long` | open-ended observation ("no planned end") selects a persistent monitor and reports its alert |  | X |  |  |  | X |  |  |  |  |  |
| `monitor-behavior-hour` | an explicit one-hour observation selects timeout_ms 3600000, not persistence |  | X | X |  | X | X | X |  |  |  |  |
| `monitor-behavior-natural` | an open-ended watch phrased naturally (no "no planned end" cue, horizon longer than the 1-hour timeout cap) selects persistent:true rather than a timed monitor |  | X | X |  |  | X | X | X |  |  | X |
| `monitor-behavior-renew` | after a short timed monitor expires, continuing an open-ended watch (horizon beyond the 1-hour timeout cap) starts a persistent monitor rather than another timed one |  | X | X |  |  | X | X | X | X |  | X |

<sub>results: `results/monitor-r2-baseline/results.jsonl`</sub>

## 2026-10-08: requested xAI/OpenAI thinking matrix

Extension tree `682206a` (base `8d4de1d`), Pi 1.0.3, baseline only, k=10. Twelve supported configurations; requested `grok-4.5:xhigh` clamps to high and is not a separate tested column. 120/120 cells complete: **1198 PASS, 2 FAIL**, with 3 INVALID and 3 ERR excluded. A supplies seven scenarios, B supplies handover/monitor, D supplies still-running; whole cells are replaced, never hand-corrected. D clarifies separate unredirected launches and parent-owned writes, and excludes same-turn completion delivery without a continuation opportunity. B handover transcripts were audited and unaffected by that added precondition. The Luna-medium stuck-child failure has ambiguous growth evidence; the Sol-high progressing-child failure has unresolved child-completion attribution. A separate 3/3 Sol diagnostic does not replace its 9/10 cell. See [campaign review and limitations](2026-10-08-REVIEW.md). No extension/product prompt changes were made.

Blank: 10 of 10 scored episodes passed. X: any FAIL, or fewer than 10 scored (INVALID and errors excluded).

| scenario | tests | `grok-4.7:medium` | `grok-4.7:high` | `grok-4.7:xhigh` | `grok-4.6:medium` | `grok-4.6:high` | `grok-4.6:xhigh` | `grok-4.5:medium` | `grok-4.5:high` | `gpt-6.1-sol:medium` | `gpt-6.1-sol:high` | `gpt-6-luna:medium` | `gpt-6-luna:high` |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `bg-end-turn` | ends its turn after a command is backgrounded instead of polling for it |  |  |  |  |  |  |  |  |  |  |  |  |
| `wake-continue` | handles a task wake and continues the work instead of only acknowledging |  |  |  |  |  |  |  |  |  |  |  |  |
| `still-running-continue` | continues from one task's wake while another background task is still running |  |  |  |  |  |  |  |  |  |  |  |  |
| `handover-continue` | continues from a per-child subagent-handover wake while the other child still runs |  |  |  |  |  |  |  |  |  |  |  |  |
| `monitor-not-sleep` | waits for a condition event-driven (monitor, or a backgrounded tail -f \| grep -m1), not with a sleep/poll loop |  |  |  |  |  |  |  |  |  |  |  |  |
| `no-fabrication` | never states a background result before its notification arrives |  |  |  |  |  |  |  |  |  |  |  |  |
| `supervisor-reply` | answers a supervisor-request wake with agent_message action reply |  |  |  |  |  |  |  |  |  |  |  |  |
| `resume-finished` | resumes a finished subagent via subagent({action:"resume"}) (an agent_message attempt first is recorded, not failed) |  |  |  |  |  |  |  |  |  |  |  |  |
| `overrun-stuck` | interrupts a subagent that overran its budget while blocked on a request that never gets a reply |  |  |  |  |  |  |  |  |  |  | X |  |
| `overrun-progressing` | lets a subagent that overran its budget but is visibly progressing finish, instead of interrupting it |  |  |  |  |  |  |  |  |  | X |  |  |

<sub>results: `results/2026-10-08a/combined.jsonl`, `results/2026-10-08b/combined.jsonl`, `results/2026-10-08d/combined.jsonl`</sub>

## 2026-10-07a: fix/extension-small-fixes (soft timeout, wake as-of)

Extension tree `1000cc0`: #45 as merged, before #41 (so without its monitor idle instruction) and before prompts moved to files. pi 1.0.3, k=10, baseline only. Harness and graders at `29fa4a6`; `overrun-stuck` at `ce8ce8a` (the first fixture's script revealed its `sleep 600`, so it was rerun on every spec with the fixture that hangs on an unanswered request). Graders before the fixes now pending: absolute-path writes (fixed in `b65fc72`, not applied to these cells), rejected calls counted as runs, an `ls`-led first look, provider stalls graded as behavior.

Blank: 10 of 10 scored episodes passed. X: any FAIL, or fewer than 10 scored (INVALID and errors excluded).

| scenario | tests | `gpt-6.1-sol:medium` | `gpt-6-sol:medium` | `gpt-6-luna:medium` | `gpt-5.6-sol:medium` | `gpt-5.6-luna:high` | `gpt-5.6-luna:max` | `gpt-5.6-terra:high` | `grok-4.7:medium` | `grok-4.6:medium` | `grok-4.5:medium` | `grok-4.5:high` | `kimi-for-coding:high` | `kimi-for-coding:low` | `k3-256k:high` | `k3-256k:low` | `deepseek-flash:high` |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `bg-end-turn` | ends its turn after a command is backgrounded instead of polling for it |  |  |  |  |  |  |  |  |  |  |  | X | X | X |  |  |
| `wake-continue` | handles a task wake and continues the work instead of only acknowledging |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| `still-running-continue` | continues from one task's wake while another background task is still running |  |  |  |  | X | X |  |  |  |  |  | X |  |  |  | X |
| `handover-continue` | continues from a per-child subagent-handover wake while the other child still runs |  |  |  | X |  |  |  |  |  |  |  |  |  |  |  | X |
| `monitor-not-sleep` | waits for a condition event-driven (monitor, or a backgrounded tail -f \| grep -m1), not with a sleep/poll loop |  |  |  |  |  |  |  |  | X |  |  | X | X | X | X | X |
| `no-fabrication` | never states a background result before its notification arrives |  |  |  |  | X |  |  |  |  |  |  |  |  |  |  |  |
| `supervisor-reply` | answers a supervisor-request wake with agent_message action reply |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |  |
| `resume-finished` | resumes a finished subagent via subagent({action:"resume"}) (an agent_message attempt first is recorded, not failed) |  |  |  |  |  |  |  |  |  |  |  | X |  |  |  |  |
| `overrun-stuck` | interrupts a subagent that overran its budget while blocked on a request that never gets a reply |  |  | X |  |  |  |  |  |  |  |  | X | X |  | X |  |
| `overrun-progressing` | lets a subagent that overran its budget but is visibly progressing finish, instead of interrupting it |  |  |  |  | X | X |  | X |  |  |  |  |  |  |  |  |

<sub>results: `results/2026-10-07a/openai/results.jsonl`, `results/2026-10-07a/openai-b/results.jsonl`, `results/2026-10-07a/openai-c/results.jsonl`, `results/2026-10-07a/xai/results.jsonl`, `results/2026-10-07a/kimi/results.jsonl`, `results/2026-10-07a/deepseek/results.jsonl`, `results/2026-10-07a/stuck2-openai-a/results.jsonl`, `results/2026-10-07a/stuck2-openai-b/results.jsonl`, `results/2026-10-07a/stuck2-openai-c/results.jsonl`, `results/2026-10-07a/stuck2-xai/results.jsonl`, `results/2026-10-07a/stuck2-kimi/results.jsonl`, `results/2026-10-07a/stuck2-deepseek/results.jsonl`</sub>
