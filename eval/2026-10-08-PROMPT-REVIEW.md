# Targeted prompt experiments — 2026-10-08

## Decision

**Keep the original product prompts.** This round tested two isolated candidates, not another grader repair:

- **C1, monitor turn yielding: rejected.** Fresh A/B scored original30/30 versus candidate29/30, with one additional candidate ERR. No demonstrated benefit justified extra wording.
- **C2, overrun extension rationale: not adopted.** Luna medium improved from8/10 to10/10 in both the formal comparison and independent confirmation. Silent-work controls passed. However, cross-model testing exposed K3 control-argument repair loops, including errors excluded from scored rates. A bounded matched attribution attempt then stopped after three consecutive candidate ERRs, before its planned sample size. The promotion gate remains unmet.

This does **not** establish that C2 caused a K3 regression. The original arm also exhibited a repair loop and recovered. Nor does a scored passing matrix establish error-free execution. No production prompt, mechanism, default scenario, grader, dependency, or model registry change is shipped from this round.

The [previous baseline matrix](RESULTS.md) and [campaign review](2026-10-08-REVIEW.md) remain historical records; neither is overwritten by these experiments. This branch starts at `7f2de3ba592b5ba9710e234889c7d31b4edc80c0` (PR #59).

## Scope and accounting

The approved scope was known monitor/overrun problems first, followed by at most two prompt hypotheses and bounded validation. Old GPT/Grok configurations, including GPT-5.x and Grok4.3, were excluded even as controls. New DeepSeek V4 and full Kimi K3 coverage were not attempted; K3 here is the explicit targeted configuration `kimi-coding/k3-256k:high`.

All requested parent model/thinking configurations were checked against actual RPC resolution. Runs used installed Pi1.0.3 and Node24.21.0. At most three parent eval episodes ran concurrently. A k3 smoke screened obvious problems, not efficacy. Formal comparisons used fresh interleaved arms with k10 scored targets, unchanged graders/default fixtures, and frozen source/manager versions. Replacements were bounded; original verdicts and excluded attempts were retained.

| Phase | Purpose | PASS | FAIL | INVALID | ERR | Attempts | Parent list-price usage |
|---|---|---:|---:|---:|---:|---:|---:|
| E | Original targeted baseline | 57 | 3 | 0 | 0 | 60 | $0.410866 |
| F | C1 smoke | 9 | 0 | 0 | 0 | 9 | $0.024217 |
| G | Fresh original/C1 A/B | 59 | 1 | 0 | 1 | 61 | $0.172080 |
| H | C2 smoke | 9 | 0 | 0 | 0 | 9 | $0.102180 |
| I | Fresh original/C2 A/B | 58 | 2 | 1 | 0 | 61 | $0.666701 |
| J | Independent Luna stuck confirmation | 18 | 2 | 0 | 0 | 20 | $0.025858 |
| K | Separate silent-work safety diagnostics | 12 | 0 | 0 | 0 | 12 | $0.020541 |
| L | Additional C2 cross-model regression | 90 | 0 | 2 | 2 | 94 | $1.793021 |
| M | K3 attribution, stopped early | 3 | 0 | 1 | 3 | 7 | $0 reported* |

**333 real-model episodes:**315 PASS,8 FAIL,4 INVALID,6 ERR. These are inventory counts across different experiments, **not a pooled success rate or causal estimate**. Parent-reported list-price usage totals **$3.215464248**. It excludes children, development/review agents and actual billing. *K3 catalog/usage prices are zero or unknown; zero does not mean the service was free.* Dry-run cost estimates were heuristics, not spending caps (L estimated ~$3.61 parent-only).

## E: targeted original baseline

| Requested configuration | Scenario | Original result |
|---|---|---:|
| `kimi-coding/kimi-for-coding:high` | monitor-not-sleep | 10/10 |
| `kimi-coding/k3-256k:high` | monitor-not-sleep | 10/10 |
| `deepseek/deepseek-flash:high` | monitor-not-sleep | 9/10 |
| `openai-codex/gpt-6-luna:medium` | overrun-stuck | 8/10 |
| `openai-codex/gpt-6-luna:medium` | overrun-progressing | 10/10 |
| `openai-codex/gpt-6.1-sol:high` | overrun-progressing | 10/10 |

All60 sandboxes and30 child transcripts were archived. The two Kimi historical monitor failures did not recur here; that does not prove their absence. DeepSeek's historical polling recurred despite an earlier passing rerun. Sol's old missing-completion failure did not recur; its original failure remains unattributed, not replaced by later passes.

DeepSeek monitor #3 retained PASS but started a redundant persistent replacement watcher before notification and stopped the original. The default grader does not prohibit that replacement, so duplicate starts were tracked separately rather than hand-regraded.

## C1: explicit turn yielding — rejected

C1 changed only `result.monitor-started-instruction` in `extension/prompts/tools/monitor.md`, replacing its initial notification sentence with:

> If all remaining work depends on this monitor, send a brief text-only reply now with no tool call. Ending the turn does not stop the monitor: it keeps running and resumes you when an event arrives as `<pi-famulus-wake kind="monitor">`. You will also receive a notice when it exits or times out.

The existing named polling/sleep prohibition and shared `monitorIdle` stayed unchanged. Only generated text and the corresponding surface snapshot accompanied the edit; seven text/surface tests passed. This tested the hypothesis that models knew to wait but did not reliably translate waiting into a tool-free response.

F smoke was9/9, one monitor each, with candidate text verified in every transcript. G then ran fresh E/F arms, k10 per model, seed20261008 and at most three concurrent parent episodes:

| Model | Original A | C1 B | Excluded attempts |
|---|---:|---:|---|
| Kimi for coding high | 10/10 | 10/10 | none |
| Kimi k3-256k high | 10/10 | 9/10 | B:1 ERR |
| DeepSeek Flash high | 10/10 | 10/10 | none |

All61 attempts reached their declared start text; neither arm duplicated monitors. DeepSeek's contemporaneous original arm did not reproduce the target failure, while C1 still allowed a genuine Kimi poll. **No demonstrated benefit; restore the original prompt.** These small samples do not establish statistically reliable harm. C1's frozen diff and failed episode remain available locally.

## C2: evidence, change and comparisons

### Attribution before tuning

The actual overrun notifier → computation → formatter/renderer was exercised with injected filesystem observations and clocks: **17 characterization readings passed**, a deliberately wrong assertion failed, and21 existing overrun tests passed. The parent independently reran this free diagnostic.

- Unchanged29 bytes at idle56619ms reports `growing=yes`;20s later it reports no. At +1s it still reports yes.
- **Recent mtime OR increased size** applies on every reading, not just the first. A same-size touch can report growth; stale-mtime size growth can also report growth. Without size growth, idle59999/60000/60001ms gives yes/no/no.
- Failed stat means **unknown**: XML omits bytes/idle/growing. It does not emit `growing=no`.
- These are counterfactual readings, not extra wakes received in the original episode. No production mechanism or grader changed.

E Luna stuck #8 supplied a stronger decision opportunity: its second delivered wake explicitly showed unchanged29 bytes and `growing=no`, yet it extended again, citing an alleged two-minute child shell limit. Child bash `timeout=120000` is actually **seconds**, converted to120000000ms. Archived events show114083ms then `stopped:tool`/SIGTERM, not a natural120s timeout. The33h20m deadline is code-derived; archived records do not persist the deadline itself.

The user task asks for an explanation if the answer is unavailable within about a minute, **not explicit cancellation**. The unchanged grader additionally expects interruption of the hung child. C2 therefore tests extension rationale, not a blanket no-growth cancellation policy or proof that every passing answer satisfies the literal user request.

### Exact candidate

C2 changes only the **no-hard-ceiling** `wakes.overrun.options` sentence:

> Extend only when progress or a known, still-useful silent workload justifies more time within the user's timing constraints; a child shell timeout is not evidence of progress or, by itself, a reason to extend past the requested timeframe.

H was frozen from **E, not the C1 worktree**. Only `extension/prompts/wakes.md`, its generated projection and corresponding surface snapshot differ. Hard-ceiling text, mechanisms, manager, default fixtures and graders are identical. The28 generated/surface/overrun checks passed; they establish text consistency and unchanged mechanisms, not model efficacy.

H smoke passed9/9: Luna stuck3/3 interrupted (one extended before recovery), Luna progressing3/3 and Sol progressing3/3 completed without interruption. All nine saw C2. These smoke samples were not pooled into the subsequent comparisons.

### I formal comparison and J independent confirmation

I used fresh interleaved E/H arms, k10 per cell, seed20261008, at most three parents. J used new files and seed20261009, one parent at a time, identical across arms. Both retained original verdicts and raw evidence.

| Experiment / model / scenario | Original A | C2 B | Excluded |
|---|---:|---:|---|
| I / Luna medium / stuck | 8/10 | 10/10 | none |
| I / Luna medium / progressing | 10/10 | 10/10 | B:1 INVALID |
| I / Sol high / progressing | 10/10 | 10/10 | none |
| J / Luna medium / stuck | 8/10 | 10/10 | none |

Successful-call/wake correlation produced these secondary **episode counts**, each out of10:

| Stuck observation | I A → B | J A → B |
|---|---:|---:|
| Any extension | 7 → 1 | 6 → 1 |
| Extension at reminder1 | 6 → 1 | 6 → 1 |
| Exposed to explicit no-growth repeat | 7 → 3 | 4 → 0 |
| Extended after such a repeat | 3 → 0 | 1 → 0 |

Raw grader `extendedFirst` means extension before interruption, **not necessarily at reminder1**. Earlier decisions affect later exposure: conditional repeat subsets are not randomized matched populations. J confirms the earlier-decision direction, **not candidate repeat-specific reasoning**, because no repeat reached J/B. I supplies only three candidate repeat-exposure episodes. Ten samples per arm do not establish reliable population improvement; do not pool selection/smoke into confirmation denominators.

All scored I/J episodes reached their declared prompt. All ten I/B stuck episodes explained the delay, but #2 said the *script* produced no output despite29 reported bytes. Others qualified this as no returned subagent result; the original arm also showed this distinction problem. The interrupt grader does not test every explanation's accuracy.

## K: useful silent work and missing observations

K reused each frozen arm's real episode runner and progressing setup/grader, with a separate diagnostic fixture. Its real85s silent job preserved the runtime random canary and final output; the parent was explicitly allowed180s, with a72s soft subagent reminder and210s harness horizon. Child bash timeout180 was in **seconds**.

| Luna medium control | Original E | C2 H |
|---|---:|---:|
| Native, explicitly no-growth silent job | 3/3 | 3/3 |
| Counterfactual missing-growth observation | 3/3 | 3/3 |

The unknown variant removed only shell bytes/idle/growing observations in the request-local context. It did not alter original history, tool declarations, candidate wording or hard-ceiling wakes. A read-only provider hook recorded the SDK payload about to be sent—not proof of network receipt or model comprehension.

Passing required actual in-flight exposure, no successful interruption, and the exact runtime result. An unexercised setup was INVALID. Requests carrying only a stale wake after producer completion or a same-run done message did not count. All12 passed without any interrupt **attempt**, and first qualifying requests preceded producer completion by15.8–18.1s. The parent independently replayed exposure/payload checks, canary matching and archived timestamps; all sealed fixture hashes matched.

Six free tests passed, including projection narrowness/immutability/hard-ceiling guards, stale-exposure rejection and the real fixture/canary path. A separate free85s lifecycle run confirmed actual silence and completion. Independent read-only evidence review found0 findings in these six tests. A development-worker timeout during fixture preparation was not an eval ERR; no paid calls occurred until preparation/review finished.

Limits: k3 controls screen known errors, not arbitrary silent workloads or natural missing-stat incidents. They explicitly tell the model silence is expected. Child transcripts were retained, but only **parent** provider payloads were audited. Timestamp fields are persisted under `metrics.diagnosticExposureAudit`; an earlier report incorrectly looked for top-level fields, and independent review corrected that documentation error without changing grades. K's sealed README retains historical pre-run wording; this report and results are the current execution record.

## L: affected-scenario regression, not clean execution

After K completed, L added90 scored candidate samples using unchanged H and original default overrun fixtures, k10 per cell, max20 attempts/cell, at most three parents. Exact model/thinking preflight passed. Only the affected two scenarios were tested, not every scenario/thinking configuration.

The candidate-only matrix below selects **whole cells**, without within-cell pooling. It is **not an accepted product baseline** or an independent improvement estimate:

| Configuration | Stuck | Progressing | Cell source |
|---|---:|---:|---|
| `openai-codex/gpt-6-luna:medium` | 10/10 | 10/10 | J/B, I/B |
| `openai-codex/gpt-6.1-sol:high` | 10/10 | 10/10 | L, I/B |
| `deepseek/deepseek-flash:high` | 10/10 | 10/10 | L |
| `kimi-coding/kimi-for-coding:high` | 10/10 | 10/10 | L |
| `kimi-coding/k3-256k:high` | 10/10 | 10/10 | L |
| `xai/grok-4.7:high` | 10/10 | 10/10 | L |

L itself is **90 PASS,2 INVALID,2 ERR**,94 attempts. All90 scored episodes saw C2; all model/thinking labels matched. All94 sandboxes and94 child transcripts were archived. The selected120-score matrix additionally carries I's excluded INVALID:125 total selected attempts, not120 error-free attempts.

All-attempt inspection found K3 stuck argument-repair loops in **3 of13 attempts**: #4/#7 failed to interrupt and ended ERR; #11 recovered after two rejected calls and remained PASS. Kimi-for-coding progressing #9 instead raced completion: a late extend was rejected because the child had finished, then the correct result was written. Do not conflate that normal race with malformed arguments.

K3 stuck #0/#5 retained PASS with `cutoffPendingResponse:true` because interruption had already succeeded. Thus four pending-response-cutoff metrics are not four ERR verdicts. ERR precedence is unchanged, but an excluded attempt can still contain observed behavioral defects. Replacements do not erase them.

Independent review initially supported conditional C2 adoption if remaining gates passed. After inspecting L's K3 repair loop, it required a matched attribution gate: the failure was on the affected path, and unchanged tool schema alone could not rule out a prompt effect.

## M: bounded attribution stopped; no adoption

M predeclared fresh E/H K3-high stuck arms, k10 scored each, seed20261010, one parent at a time, max20 attempts/arm and a three-consecutive-ERR stop. It reused the unchanged runner, fixtures, classifier and source hashes. Secondary auditing covered **all attempts**: rejected controls, repeated repair, recovery, inspection/probe calls and cutoffs. No third candidate or mechanism repair was introduced.

The controller exited1 with **`B: three consecutive errors`**, as designed:

| Arm | Attempts | PASS | INVALID | ERR | Malformed-interrupt episodes | Recovered |
|---|---:|---:|---:|---:|---:|---:|
| A original | 4 | 3 | 1 | 0 | 1 | 1 |
| B C2 | 3 | 0 | 0 | 3 | 2 | 0 |

**This is incomplete, not a k10 comparison or a candidate0% scored pass rate.** Original A/#0 had two malformed interrupts before a successful one near the nominal horizon. That establishes baseline susceptibility, not equal rates or candidate exoneration. B/#0 looped without any diagnostic detour; B/#1 was latency-heavy without a malformed call; B/#2 combined diagnostics with rejected interrupts.

The three-error guard was not relaxed and the study was not restarted. **C2 remains unpromoted:** replicated Luna observations do not resolve the K3 execution concern. The data do not support a statistical causal-regression claim either. Keep original product text rather than add another candidate or continue sampling until the gate looks green.

## Nonpass and qualitative evidence ledger

Original grades remain unchanged. Case numbers are local to their phase/model/scenario/arm.

| Case | Verdict | Observed evidence / limit |
|---|---|---|
| E / DeepSeek monitor #0 | FAIL | Successful monitor at3.14s; waiting reply appended `sleep 5; cat service.log` at4.33s. Guard rejected it. Correct eventual READY answer does not undo the attempt. Raw replay through the unchanged grader reproduces FAIL. |
| E / Luna stuck #8 | FAIL | First ambiguous growth wake → extend30s. Second at100.96s: unchanged29 bytes, idle94088ms, growing=no → another30s extension, citing a nonexistent two-minute shell limit. Explained missed timeframe at73.32s. No early9s deadline claim in this episode. |
| E / Luna stuck #9 | FAIL | First growth=yes wake at77.07s → extend60s at83.55s; next reminder beyond horizon. Initial-signal/timing confound, not repeat evidence. |
| G / B / K3 monitor #1 | FAIL | `grep READY service.log` at19.33s before queued wake delivery at19.35s, after actual C1 start text. |
| G / B / K3 monitor #5 | ERR | READY arrived after a tool-free acknowledgement; next response remained pending. Full top-level error overrides nested pass:false. Initial truncated-wake misreading was corrected, not the grade. |
| I / B / Luna progressing #0 | INVALID | Parent chose1800000ms budget; child finished35.65s and correct file was written. No overrun/C2 exposure. |
| I / A / Luna stuck #6 | FAIL | First wake acknowledged without control; second at84.51s showed29 bytes/idle76726ms/growing=no. Extended30s at90.92s “in case it finishes.” |
| I / A / Luna stuck #9 | FAIL | First ambiguous growth wake → extend60s at66.08s, next reminder beyond horizon. Explained delay at73.62s. |
| J / A / Luna stuck #1 | FAIL | Extended30s twice. Second wake at96.97s showed29 bytes/idle88626ms/growing=no; extended again98.96s while acknowledging no new output. |
| J / A / Luna stuck #6 | FAIL | First growth=yes wake → extend60s at68.55s, next reminder beyond horizon; repeated-no-growth decision not observed. |
| L / DeepSeek stuck #4 | INVALID | Read script, probed endpoint for5s, interrupted15.87s before60s budget; never saw C2. Claimed inability to finish within a minute at18.06s and extrapolated “never responds” from a short probe. |
| L / K3 stuck #4 | ERR | After C2 and diagnostics, four `interrupt` calls with nonempty `tasks` were rejected97.86–116.85s. Three wakes, no successful interruption. Not a pure infrastructure-only stall. |
| L / K3 stuck #7 | ERR | Same malformed-argument pattern, six rejections77.25–114.92s, no successful interruption. |
| L / K3 stuck #8 | INVALID | First15s-budget wake had no child shell. Later C2 shell exposure cannot repair that precondition. Eventually interrupted91.47s; also used a deadline monitor, probes and task-list query. |
| M / A / K3 stuck #3 | INVALID | Selected120000ms budget at7.41s, moving first overrun beyond the episode horizon. Child did not naturally finish; generic “child ended before its budget” wording is misleading here. No candidate text. |
| M / B / K3 stuck #0 | ERR | Five malformed interrupts80.79–114.25s, no inspection/probe detour, no recovery. C2 reached three wakes. |
| M / B / K3 stuck #1 | ERR | Start delayed35.07s, acknowledgement66.92s, first wake95.11s; read output/script119.04s, then pending response. No malformed interrupt in observed prefix. |
| M / B / K3 stuck #2 | ERR | Output/script/socket/HTTP diagnostics, then malformed interrupt115.64s and another retained121.00s near shutdown. No credited interruption. |

Additional retained PASS caveats: E DeepSeek monitor #3 duplicated a watcher; I/B Luna stuck #2 confused shell output with child result; L K3 stuck #11 recovered from two malformed calls at87.76s; M/A K3 stuck #0 recovered near121.06s and retained pending-response metadata. Successful credit follows the actual pre-shutdown prefix, not an offline reinterpretation of a nominal120s wall-clock boundary.

Some raw diagnostics contain unrelated host process information. They stay local; this report deliberately does not publish those inventories. A model diagnostic read is not automatically a polling violation merely because a coarse metric labels it that way; preserve the calls and their notification context.

## Provenance and verification

Raw artifacts are gitignored under `eval/results/`. Large transcripts, request payloads, sandbox contents and child logs are not committed. Every measured source remains sealed; coding-agent worktrees were not used as moving baselines. Auxiliary K diagnostics are separate from default baseline grading.

| Phase | Local artifact directory |
|---|---|
| E | `2026-10-08e-problem-baseline/` |
| F | `2026-10-08f-monitor-candidate/` |
| G | `2026-10-08g-monitor-ab/` |
| H | `2026-10-08h-overrun-candidate/` |
| I | `2026-10-08i-overrun-ab/` |
| J | `2026-10-08j-overrun-confirm/` |
| K | `2026-10-08k-silent-controls/` |
| L | `2026-10-08l-overrun-regression/` |
| M | `2026-10-08m-k3-attribution/` |

Frozen source SHA-256:

- E original: `54320c3e45f82bd56a5b6401482284e5fa96f2f105c45c7bd6ff5efe4beb1d3a`
- F C1: `6512821811582dfcc53593a916ae6aad4d15872ae13a161b06269d60dd9ffff0`
- H C2: `76617d6048db0cd8237eda6ecd4a825e47d34a8a6b5b2a082c008079d9cee0aa`
- Shared manager: `fd20074fbba4cd9c60976e1a2ac6ebd8254211b39ae3d8a19f561002cc8ab946`

Snapshots contain per-file hashes; campaigns retain plans, preflights, launch/sandbox mappings and raw verdicts. G/I/J/M retain arm-specific combined files and summaries. I/J have accepted-action/wake audits; K has context/provider and independent timestamp audits; L/M have all-attempt behavior audits. `2026-10-08-prompt-final/summary.json` records333-episode inventory input paths/hashes and costs. `2026-10-08-overrun-characterization/` retains deterministic readings, rendered wakes and independent findings.

During experiments:7 C1 text tests,28 C2 text/overrun tests,17 deterministic overrun readings plus21 existing tests, and6 K fixture tests passed. These checks do not establish model efficacy. Final free repository verification passed after rebasing onto PR #59's unchanged head: eval typecheck,100 unit tests,16 faux E2E tests and29 faux grader tests; extension typecheck and720 tests (13 opt-in integration tests skipped). `git diff --check` passed, and `git diff --exit-code -- extension` confirmed the product tree stayed at its original state. These are fresh local results, not a claim about CI or the skipped integration tests. Independent final read-only review verified raw inventory hashes/counts/costs, the125 selected candidate attempts, all seven M cases, source seals and the local verification log; it found no factual or privacy blockers to publishing this report.

## Follow-up boundaries

- K3 malformed action repair, latency-sensitive censoring and explanation accuracy need separate investigation. Baseline susceptibility is now observed; prompt causation remains unresolved. Do not silently broaden this round into a third candidate or grader/schema fix.
- The old Sol missing-completion incident remains unattributed. New passes do not replace it.
- No full prompt ablation, all-thinking sweep, real opt-in monitor/UI synthetic-long probes, or new-model full sweep was performed here.
- Commit the report through Commitizen and open a separate PR without automatically merging. Candidate text is preserved above for future controlled work, not adopted as product policy.
