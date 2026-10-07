# Eval results

One section per run of the baseline prompt (smoke tier), newest first. Rows are scenarios (their prompt and what they test: [`scenarios/`](scenarios/)); columns are model specs exactly as `pi --model` takes them (`id:thinking`). Every cell is the grader's verdict, with no hand corrections: when a verdict is wrong, the grader or the scenario is fixed and the cell is rerun.

Generate a section from the run's result files (later files replace earlier ones cell by cell, e.g. a rerun of one scenario):

```bash
node ablation/matrix.ts --results results/<run>/a.jsonl,results/<run>/b.jsonl \
  --title "<date>: <what changed>" --note "<extension tree, harness commit, notes>" --write RESULTS.md
```

When to rerun and with which models: [BASELINES.md](BASELINES.md#when-to-rerun).

<!-- runs: newest first -->

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
