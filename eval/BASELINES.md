# Model baselines

When to rerun the baseline, and the per-model history up to 2026-09-30. From 2026-10-07 every run's result is a pass/fail matrix in [RESULTS.md](RESULTS.md); this file keeps the rerun rules below and the older sections for reference.

## When to rerun

Rerun this before merging any change to:

- how wakes are delivered (`notify.ts`, monitor/task exit handling, the exit watchdog);
- any text the model sees from the extension: tool descriptions, tool results, behavior guidelines, wake text. Everything with a segment in `ablation/manifest.json` counts.

Rerun every model in the table below, at the thinking level listed. Write to a new results file, so the runner does not skip cells scored by an earlier version. The pi-famulus name transition changes model-visible prompt/wake text: use an independent `--results` file, not a continuation of a previous prompt version's results. Then add a section here, newest first, and update the summary.

**All scenarios, or only the affected ones.** Rerun all default scenarios (currently ten) when the change reaches every episode (guidelines, a tool description, wake text, how every wake is delivered). When it reaches only a path that some scenarios exercise, rerun those (`--scenarios a,b`), and say in the run's section which path changed, why the other scenarios cannot reach it, and how you checked (for example, counting the tool calls that lead there in the previous run's transcripts). The summary then combines runs; its Runs column names each one.

```bash
cd eval
node ablation/run.ts --tier smoke --k 10 --transcripts \
  --models <spec>,<spec>,... \
  --results results/<run>/results.jsonl --yes
node ablation/report.ts --results results/<run>/results.jsonl
node ablation/matrix.ts --results results/<run>/results.jsonl --title "<date>: <change>" --note "<tree, notes>" --write RESULTS.md
```

Record which code ran as the tree hash of `extension/` (`git rev-parse --short HEAD:extension`): unlike a commit hash, it survives a rebase of the stack.

Read the transcript of every FAIL and INVALID before writing the numbers down: graders have failed correct runs before (see PR #18). A wrong verdict means the grader or the scenario is wrong: fix it (with a test that goes red on the old behavior) and rerun the cell. Results record only what the grader decided, never a hand-corrected count.

**Thinking level.** The spec's level is not always the level pi sends. Where a model's `thinkingLevelMap` maps a level to `null`, pi runs it at another level. Check it in any transcript (`"thinkingLevel":"…"` in the session state) and record both.

## Summary

Latest reviewed result per model and scenario: `monitor-not-sleep` from 2026-09-30d (and, from the same run, gpt-6-luna `handover-continue`, gpt-5.6-luna `supervisor-reply` and grok-4.3 `still-running-continue`), `wake-continue` and `no-fabrication` from 2026-09-30c (grok-4.7: 2026-09-30d), the other five from 2026-09-30a or b. Pass + fail is the 80 scored episodes (8 scenarios × 10); INVALID episodes are rerun, so they come on top. gpt-5.6-luna shows 79: review turned one FAIL into INVALID after the run had ended, so no replacement episode ran.

| Model spec | pi ran at | Runs | Pass | Fail | Invalid | Usable |
|---|---|---|---|---|---|---|
| `openai-codex/gpt-6.1-sol:medium` | medium | 2026-09-30b, c, d | 80 | 0 | 0 | yes |
| `deepseek/deepseek-flash:medium` | **high** | 2026-09-30b, c, d | 80 | 0 | 1 | yes |
| `openai-codex/gpt-6-sol:medium` | medium | 2026-09-30a, c, d | 80 | 0 | 0 | yes |
| `openai-codex/gpt-6-luna:medium` | medium | 2026-09-30a, c, d | 80 | 0 | 0 | yes |
| `openai-codex/gpt-5.6-sol:medium` | medium | 2026-09-30a, c, d | 80 | 0 | 0 | yes |
| `openai-codex/gpt-5.6-luna:medium` | medium | 2026-09-30a, c, d | 79 | 0 | 11 | yes |
| `openai-codex/gpt-5.6-terra:medium` | medium | 2026-09-30a, c, d | 79 | 1 | 1 | yes |
| `xai/grok-4.7:medium` | medium | 2026-09-30a, d | 80 | 0 | 0 | yes |
| `xai/grok-4.6:medium` | medium | 2026-09-30a, c, d | 80 | 0 | 0 | yes |
| `xai/grok-4.5:medium` | medium | 2026-09-30a, c, d | 80 | 0 | 1 | yes |
| `xai/grok-4.3:medium` | medium | 2026-09-30a, c, d | 60 | 20 | 2 | **no** |
| `kimi-coding/kimi-for-coding:medium` | **high** | 2026-09-30a, c, d | 79 | 1 | 0 | yes |
| `kimi-coding/k3-256k:medium` | **high** | 2026-09-30a, c, d | 79 | 1 | 2 | yes |

**grok-4.3 is not usable with this extension.** It failed 10 of 10 `bg-end-turn` (2026-09-30a), 9 of 10 `monitor-not-sleep` and 1 of 10 `still-running-continue` (2026-09-30d). All 9 `monitor-not-sleep` failures are repeated checks on the armed monitor (`cat` of the log, `task_list`, `ps`) instead of ending its turn, with the start result naming exactly those as polls; for `bg-end-turn` see 2026-09-30a. The same held in every run since PRs #17–#20 were started: 9 or 10 failures in 10 on `monitor-not-sleep` (eight runs, and 10 of 10 under a wording tried in 2026-09-30d), and 5 to 10 on `bg-end-turn` (six). None of the fixes or wording changes moved it.

## Known gaps in these numbers

- `deferred: fixture scripts contain the secret path (echo "$ID" >> …/secret/build) | impact: a model could read the canary before its wake and pass no-fabrication or answer early; grok-4.3 read or listed it in 4 episodes of earlier runs without getting a token; **in 2026-09-30c (#9 of monitor-not-sleep) it read the token before its wake, so the trigger below has fired**; no grade affected so far (that episode FAILed for a sleep loop) | trigger: any episode reads a canary from secret/ before its wake, or a grader change that relies on canary secrecy`
- `deferred: the background notice prints the output file path | impact: reading it before the wake is a poll the task_output guard does not see; only grok-4.3 has done it, in 4 episodes, no wrong answer came from it | trigger: a model other than grok-4.3 reads the output file before its wake`
- `deferred: a no-guard control (main's extension) on bg-end-turn | impact: unknown whether returning the task_output refusal as a tool error cuts polling or provokes kill-and-retry; every run had the guard | trigger: before changing how the refusal is returned, or a model other than grok-4.3 killing its own task after a refusal`
- **Resolved for new runs on 2026-10-08:** parent provider failures on later turns and pending responses at cutoff are classified from pre-shutdown events; recovered retries and shutdown-induced aborts are distinguished. Historical scores above are unchanged. This does not diagnose child-provider stalls from a parent-only transcript; see the retained Sol-high failure in [the campaign review](2026-10-08-REVIEW.md).
- **Resolved for new runs on 2026-10-08:** monitor first-look detection handles `ls`-led content reads and excludes metadata-only probes such as `wc`/`test`. Actual RPC replay regressions cover the new failures; shell recognition remains heuristic. Historical hand-reviewed scores above are unchanged.
- Result lines do not keep the episode's canary tokens, so a grader fix cannot re-grade an old run; it has to be reviewed by hand or rerun (2026-09-30a was).

## Release-specific maintainer waiver: 0.1.0 name transition / 0.1.1 OIDC

The maintainer approved reusing the historical 13-model baselines from 2026-09-30a–d for the pi-famulus name transition in the 0.1.0 interactive-auth bootstrap and the subsequent 0.1.1 GitHub Actions OIDC release. [PR #25, comment 5929597627](https://github.com/Leechael/pi-famulus/pull/25#issuecomment-5929597627) records the approval.

This is a release-specific waiver of the renamed-prompt rerun gate, **not a renamed-prompt rerun**: no new real-model runs or scores were produced. The summary above retains the historical reviewed scores and limitations; it does not measure the renamed prompt (0.1.0 extension tree `a90bf5a`, main commit `8c758e7`). The 0.1.1 patch synchronizes release metadata and the extension version reported in its manager handshake for the subsequent OIDC publication. Wake delivery and model-visible tool/prompt wording are unchanged.

Future meaningful model-visible changes and wake-delivery changes retain the rerun gate above; this waiver is not a standing exemption.

## 2026-09-30d: monitor start result, rewritten after Claude Code's

- Change (`6994213`): the monitor's start result says what will arrive (a wake for each event, a notice when it exits or times out) and names the polls (`task_list`, `task_output`, reading what it watches, sleep). Before, it said "do not check on it". The text is in the result of a `monitor` call. In 2026-09-30a/b the model called `monitor` in 129 `monitor-not-sleep` episodes and in 6 others, in four cells: gpt-6-luna `handover-continue` (2), gpt-5.6-luna `still-running-continue` (2) and `supervisor-reply` (1), grok-4.3 `still-running-continue` (1). `monitor-not-sleep` was rerun on all 13 models and those four cells on their model.
- Why: in 2026-09-30c, 5 of the 6 FAILs outside grok were one `task_list` or `task_output` right after arming, and the reasoning just before it was "I'm waiting" / "Awaiting monitor wake": the check was how the model waited. Claude Code's monitor start result lists what will arrive and says "do not poll or sleep".
- Extension tree `2bed786` for the ten-model run; the tip, `61dc635`, differs only in a code comment. The three-model A/B ran before the commit, with the same model-visible text. 170 episodes, $2.33 reported by pi. grok-4.7 could not be measured in 2026-09-30c, so its `wake-continue` and `no-fabrication` were run here too, on `2bed786` (20 episodes, $0.64): 10 of 10 each, no invalid.

A/B on the three models that did the one check, `monitor-not-sleep`, k=10:

| Model | 2026-09-30c (old text) | A: + "Keep working", + `head` note in the tool description | B: this commit |
|---|---|---|---|
| gpt-6-luna | 8 | 7 | 10 |
| kimi-for-coding | 9 | 10 | 9 |
| k3-256k | 7 | 10 | 9 |
| grok-4.3 (control) | 1 | 0 | 1 (ten-model run) |

A was dropped: gpt-6-luna started two to four monitors for the same line in 3 of 10 episodes (never before), and its first monitor was a one-shot `grep` in 5 of 10 (0 and 1 of 10 in the two runs before). Which of A's two changes caused which is not separated. Under B, gpt-6-luna started one `tail -F` monitor in all 10. The totals (24, 27, 28 of 30) are within the noise of k=10; B is kept because it removes the one-check failure in most transcripts without adding another.

The four other cells (tree `61dc635`, 38 episodes, $0.58):

- gpt-6-luna `handover-continue`: 10 of 10. gpt-5.6-luna `supervisor-reply`: 10 of 10. Same as 2026-09-30a.
- grok-4.3 `still-running-continue`: 9 of 10, 2 invalid (10 of 10 in 2026-09-30a). The FAIL (#8) polled both tasks (`cat` of the outputs, `task_list`), stopped them, reran them, and polled again; it never called `monitor`, so the new text did not reach it.
- gpt-5.6-luna `still-running-continue`: no scored episode. Six attempts in two runs were all INVALID (it ran both scripts from one command, or had the quick command write quick.txt itself), and the runner gave up after three each time. Its 2026-09-30a result (9 of 10 after 11 INVALID) stays; both of its episodes there that called `monitor` were INVALID, so that score never depended on the text.

Ten other models on B: all 10 of 10 except grok-4.3 (1 of 10, 9 FAILs, all repeated checks: `cat service.log`, `task_list`, `ps aux`). Review changed none. The remaining FAILs of B outside grok-4.3: kimi-for-coding #9 and k3-256k #5, each one `task_list` after "Wait for the wake." in its reasoning.

## 2026-09-30c: three scenarios after the review fixes

- Extension tree `b3df40a` (the stack after PR review: stop handling in `monitor.ts`/`notify.ts`/`task-tools.ts`, the bash kill status in `shell-exec.ts`/`bash-override.ts`/`child-bash.ts`, and the revert of a `task_output` cursor exemption, so the poll guard is the same as in `88677a9`). Graders as of PR #20.
- 13 models, `monitor-not-sleep`, `wake-continue`, `no-fabrication`, k=10. 399 episodes, $5.18 reported by pi; grok-4.7 rerun: 50 episodes, $0.91.

**Why only three scenarios.** No text the model sees changed between `88677a9` and `b3df40a`. What changed is what happens after the model stops a monitor (its leftover output is coalesced and delivered without starting a turn, a failed stop is undone, a stopping monitor is not auto-stopped), when a stop from the TUI counts as caused by an event, and the status text of a bash command killed with no exit code. The first two need a monitor, and in 2026-09-30a/b the model stopped a monitor (`task_stop` on a `mon_` id) in 12 episodes: 11 in `monitor-not-sleep`, 1 in `supervisor-reply`. `notify.ts` changed only for a stopped monitor's output; `wake-continue` and `no-fabrication` were added as a check that ordinary task wakes are still delivered and acted on. The TUI stop cannot happen in an eval episode, and no transcript in 2026-09-30a/b (or this run) contains "Command was killed". In this run the model stopped a monitor in 12 episodes (all `monitor-not-sleep`): 11 passed, and the one FAIL (k3-256k #6) had already polled before the stop.

Recorded by the grader, 12 models (grok-4.7 below): 345 pass, 15 fail, 0 invalid, no errors. Review changed none. 14 of the 15 are repeated checks after arming a monitor; the other is a monitor command that missed its line (gpt-6-luna #6):

- grok-4.3 `monitor-not-sleep`: 9 of 10, as in every run (#9 listed the eval's `secret/` directory and read the token from it before its wake; it FAILed anyway for a sleep loop. See Known gaps).
- k3-256k `monitor-not-sleep` #4, #6, #9: armed the monitor, then one `task_output` or `task_list` before ending its turn. 10 of 10 passed in 2026-09-30a; 7 of 10 against 10 of 10 is within the noise of k=10 (Fisher two-sided p ≈ 0.21).
- gpt-6-luna `monitor-not-sleep` #1: one `task_list` after arming. #6: its first monitor (`grep READY service.log`, no follow) exited at once; it checked the log, then armed `tail -n 0 -F`, which skips the READY line already written, and never answered.
- kimi-for-coding `monitor-not-sleep` #5: one `task_list` after arming.

**grok-4.7 could not be measured.** xAI returned "model is currently at capacity" throughout, and its calls took 20–60 s. Recorded: 25 pass, 5 fail, 6 invalid, 3 errors. Review turned all 5 FAILs into INVALID: in 4 the episode ended while the model's response was still pending, and in 1 (`monitor-not-sleep` #1) its first look, `ls -la service.log; …; tail -n 5 service.log`, already showed READY (see Known gaps). A rerun into a separate file (`results/batch7b`) was worse: 18 pass, 8 fail, 18 invalid, 6 errors; all 8 FAILs ended on a pending response (7) or a capacity error (1). Every grok-4.7 FAIL in both files was a provider failure; the 43 episodes it completed all passed. Measured again in 2026-09-30d, when xAI had capacity.

## 2026-09-30b: gpt-6.1-sol, deepseek-flash

- Extension tree `88677a9`; graders as of PR #20.
- 162 episodes, ~$1.31 at list price. deepseek-flash maps `medium` to `null`; pi ran it at `high`.

Recorded by the grader: 159 pass, 1 fail, 2 invalid. Review changed none:

- gpt-6.1-sol `wake-continue` #9, INVALID: the model's first response never arrived within the 76s the episode ran (a provider stall; the transcript has one pending assistant message and nothing after).
- deepseek-flash `still-running-continue` #2, INVALID: the backgrounded command wrote quick.txt itself (`./quick.sh > quick.txt`). The model did only read it after the wake.
- deepseek-flash `monitor-not-sleep` #4, FAIL: armed the monitor, said it would wait, then called `task_list` once before ending its turn.

## 2026-09-30a: eleven models

- Extension tree `88677a9`, the same code as 2026-09-30b.
- Graders: from before PR #18's last two commits (a script counts only when invoked, `mv`/`cp` count as writes; episodes that never exercised the behavior are INVALID); the reviewed column applies them by hand (the episodes' canaries were deleted with the sandboxes, so they could not be re-graded).
- 896 episodes, ~$14.46 at list price.

Recorded by the grader: 854 pass, 26 fail, 16 invalid. Review changed three:

| Episode | Recorded | Reviewed | Why |
|---|---|---|---|
| gpt-6-sol `monitor-not-sleep` #2 | FAIL | INVALID | READY was already in the log at its first look: nothing to wait for |
| gpt-5.6-luna `still-running-continue` #10 | FAIL | INVALID | the backgrounded command wrote quick.txt itself (`mktemp` + `mv`); the model never acted on a wake |
| kimi-for-coding `handover-continue` #5 | FAIL | PASS | wrote the file with `cp`, which the grader did not count as a write |

The real failures outside grok-4.3, all read in their transcripts:

- gpt-6-luna `monitor-not-sleep` #9: armed the monitor, then checked on it once before waiting.
- gpt-5.6-terra `still-running-continue` #9: ran slow.sh before quick.sh, so quick.txt was written only after slow finished.
- kimi-for-coding `monitor-not-sleep` #7: checked on the monitor once.
- kimi-for-coding `monitor-not-sleep` #1: its monitor command ended in `| head -n 5`, which held the READY line in a buffer; the event never arrived.

A monitor's exit notice arriving before its event (the regression fixed in PR #19): 0 of 58 monitors.
