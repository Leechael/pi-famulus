# 2026-10-09: candidate P control-mode clarification

**Status: targeted validation passed; adoption/merge scope gate pending.** Candidate P is not adopted or merge-ready. Final review has verified the targeted evidence, but [BASELINES.md](BASELINES.md#when-to-rerun) requires all ten default scenarios across its listed models for globally visible tool descriptions. Q/R do not satisfy that gate. No new maintainer waiver has been granted; adoption requires that decision or the prescribed rerun. The user requested the full round; it is planned/in preparation, not yet running or passed. The historical release-specific waiver is not a standing exemption.

## Proposed product change

- Retain C2's no-hard-ceiling rationale: extend for progress or a known useful silent workload within the user's timing constraints; a child shell timeout alone does not justify extension.
- Clarify `subagent` parameter descriptions: `tasks`/`chain` launch new work only when `action` is omitted; control calls omit both fields, not placeholder tasks.
- Only `extension/prompts/{wakes.md,tools/subagent.md}`, their generated projection and prompt-surface snapshot change. No schema shape, runtime, manager, default fixture/grader, dependency or hard-ceiling change. The proposed four files match sealed P byte-for-byte.
- N's verbose wake repair paragraph was rejected and removed. O was never started; it provides no efficacy evidence. The historical nonadoption decision in [PR #61](https://github.com/Leechael/pi-famulus/pull/61) is unchanged; this base does not contain that PR's report.

## Separate rounds, original verdicts

| Round | Purpose | Attempts | Original records |
|---|---|---:|---|
| N | Rejected verbose wake candidate smoke | 10 | 9 PASS, 1 ERR |
| P | Parameter-description candidate K3 stuck smoke | 5 | 3 PASS, 2 ERR |
| Q A | Fresh E baseline confirmation | 34 | 27 PASS, 3 FAIL, 1 INVALID, 3 ERR |
| Q B | Fresh P candidate confirmation | 30 | 30 PASS |
| R | P-only finite-silent safety controls | 6 | 6 PASS |

These are separate denominators: do not pool smoke, historical experiments or confirmation. P's two retained pending-response smoke errors had setup/timing limitations; their cause is not claimed resolved, and they are not proof of prompt improvement. Original grader verdicts and errors were not hand-corrected; supplemental audits remain separate.

### Q: three targeted cells

Requested and actual models match: `kimi-coding/k3-256k:high` (`anthropic-messages`) and `openai-codex/gpt-6-luna:medium` (`openai-codex-responses`). Fresh per-episode runtime canaries; randomized A/B order seed **20261011**. Target k10 scored/arm/cell, max14 attempts/arm/cell, stop after three consecutive ERR, global maximum three parent episodes. Original default scenarios, harness and manager were unchanged.

| Cell | A: E baseline | B: candidate P |
|---|---|---|
| K3 stuck | 10 PASS + 1 INVALID + 1 ERR | 10 PASS |
| K3 progressing | 10 PASS | 10 PASS |
| Luna stuck | 7 PASS + 3 FAIL + 2 ERR | 10 PASS |

The narrower control-mode signal differs from final PASS rates: A K3 stuck mixed `action` with nonempty launch fields in **5/12 attempts** (#3, #8, #9, #10, #11), causing **10 rejected calls**. Four recovered to PASS; #8 had six repeated rejected calls and ended ERR. B K3 stuck had **0/10** such attempts. Attempt identifiers here are the runner's zero-based `attempt` values.

Independent review checked **381 actual SDK schema projections**, **89 action calls** and their raw arguments. All 64 Q attempts exposed the appropriate arm/model schema, including P's actual parameter descriptions. Local schema tests alone were not used as exposure proof. The comparison is small, targeted and evaluates C2 plus parameter clarification together, not each sentence's isolated causal effect.

Exceptions retained despite PASS or generic grader text:

- A K3 stuck #4 remains INVALID: its explicit 120s launch budget pushed the required wake beyond the horizon; the generic grader explanation is misleading.
- A K3 progressing #8 attempted a late extend after the child completed; the rejection is an ordinary completion race, not launch/control mixing.
- B Luna stuck #9 attempted invalid `subagent` action `send`, then corrected to `agent_message` plus interrupt. Zero launch/control mixing does not mean zero rejected controls.
- B Luna stuck #3 incorrectly described zero script output. Explanation accuracy is outside the grader's PASS contract.
- B K3 stuck #5 proved PASS while a response was pending at cutoff, with no final diagnostic explanation. Pending-response flags are not silently converted to ERR or erased.

### R: legitimate silence and missing-stat counterfactual

P only, Luna medium: **3/3 native silent + 3/3 unknown-stat PASS**. Both controls used the same sealed P source/manager, K's unchanged fixture/projection/grading seams, an actual **85s** silent job, explicit **180s** allowance, **72s** soft budget and **210s** harness horizon. Max5 attempts/control, three-consecutive-ERR stop, one sequential parent; this is not an A/B efficacy comparison or a default-scenario rerun.

Independent archive replay verified all six exact runtime final-line canaries, **zero interrupt attempts** (including rejected/unconfirmed attempts), actual P schema exposure and the missing-stat-only projection. Exposure required a genuine provider request before producer completion; same-run completion/stale wakes were excluded. Original base/diagnostic grades are preserved separately. Missing stats are a request-local counterfactual, not a production telemetry change.

## Frozen provenance and free checks

Source SHA256 (full sealed source, not merely commit identity):

```text
N:       563e02dd6e9b7f586e963f9389f54304bf2c71c16389840b1493c9e387d5e9ad
E / Q A: 54320c3e45f82bd56a5b6401482284e5fa96f2f105c45c7bd6ff5efe4beb1d3a
P / Q B / R: 16260e92d5b554b749c63d944f0277430c3501715e13327231e7dd33a8716481
manager: fd20074fbba4cd9c60976e1a2ac6ebd8254211b39ae3d8a19f561002cc8ab946
```

Runtime: Pi 1.0.3, Node v24.21.0. N/P smoke targeted k3/cell, max5 attempts/cell and three-consecutive-ERR stop. Q/R used fresh roots and caps above, not resumed smoke files. R reused Q's metadata-only Luna preflight and checked actual episode model/thinking.

Unchanged P sources passed **733 extension tests** (including 13 manager/startup integration tests), **100 eval unit**, **16 faux E2E**, **29 scripted-grader** and **4 faux TUI** tests; both typechecks and generated-prompt check passed. These are free local checks, not proof of real-model generalization or candidate CI status.

## Recorded parent usage cost, not billing

Sum of actual `usage.cost` records, including errors/invalids, in catalog list-price USD:

| Records | Parent list-price total |
|---|---:|
| N smoke | $0.00856462 |
| P smoke | $0.00000000 |
| Q A / Q B | $0.01730370 / $0.01213100 |
| R | $0.01079324 |
| All 85 attempts, accounting only | **$0.04879256** |

These totals exclude child sessions, development/validation calls and actual billing; OAuth/subscription charges may differ. K3's zero catalog rate is **not free inference**. Aggregating cost does not pool behavioral scores.

Raw records, original verdicts, transcripts, sealed sources, request audits and archived child evidence remain local/gitignored under `eval/results/2026-10-09{n-overrun-control-candidate,p-control-mode-candidate,q-control-mode-ab,r-control-mode-safety}/`. They are not bundled with this report; a clean checkout cannot reconstruct these recorded episodes without those artifacts or new authorized runs. No credentials, transport signatures, host inventories, absolute host paths or canary values are published here.
