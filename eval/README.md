# eval/ — does each prompt and mechanism earn its place?

Two layers, one harness:

| Layer | What it tests | Model | Cost |
|---|---|---|---|
| **E2E** (`e2e/`) | The code: real `pi` + real `pi-famulus` + this extension, driven by a scripted faux model | faux | free |
| **Ablation** (`ablation/`) | The model: remove one prompt segment or mechanism at a time and measure the pass-rate drop on small behavioral probes | real models | see below |

Every episode spawns the installed `pi` in RPC mode (`pi --mode rpc -ne -ns -np -nc --no-session --offline -e ../extension …`) in a temp cwd with its own `PI_FAMULUS_HOME` (own manager socket, `config.json`, task logs). RPC mode rather than `-p`: print mode exits as soon as the first run settles, so later wakes would never be seen.

## Prerequisites

- Node ≥ 22.18 (runs `.ts` directly; no build step)
- `pi` on `PATH` (tested with 1.0.0) — override with `PI_BIN`
- `cargo`: `pi-famulus` is built from `../manager` into `eval/.cache/target` on first use (never inside `manager/`); override with `PI_FAMULUS_MANAGER_PATH`
- `tmux` for the TUI test
- Dependencies in `eval/` (and `extension/`) are required for typechecking, unit tests, deterministic compatibility tests, and eval runners; fixture imports use pinned runtime dependencies. Locally, install them with `npm run deps`, which runs [nub](https://github.com/nubjs/nub) (`nub install --frozen-lockfile --node-linker hoisted`) from the same `package-lock.json`. CI keeps `npm ci`.

  Why nub: `@earendil-works/pi-coding-agent` alone is ~430 MB, and npm writes a full copy into every checkout. nub links files from one global store (copy-on-write clones on APFS), so each extra worktree costs a few MB instead of ~470 MB (measured 2026-10-08: a second `eval/` install took 6 MB of disk with nub, 472 MB with npm). The hoisted layout is required: the tsconfig `paths` entry and some tests reach the SDK's own nested `pi-ai`, which nub's default isolated layout does not expose.

Extension under test: `../extension` (override with `PI_FAMULUS_EVAL_EXTENSION` only for post-transition revisions compatible with this harness). For earlier revisions, use their matching harness and manager as described in [Where results go](#where-results-go).

## E2E (free)

```bash
npm run test:e2e      # faux scenarios + ablation-harness self-test   (~15s)
npm run test:tui      # PI_FAMULUS_E2E_TUI=1: tmux-driven interactive pi, asserts no line wider than the pane
npm run test:unit     # sandbox socket paths, wake adapter, graders, stats, and report verdicts
node --test ablation/monitor-waiter.test.ts  # deterministic compatibility fixture/graders; no model calls
npm run test:graders  # real-model graders run on scripted good/bad behaviors (~2 min)
npm run typecheck
```

`test:graders` proves each grader can go both green and red (a grader that cannot fail is a placebo). The `supervisor-reply/send` case deliberately takes ~2 minutes: the child stays blocked until the episode cap.

Faux scripts live in `e2e/scripts/`; the DSL is `e2e/faux-dsl.ts`. Scripts run inside the pi process, and every model call (with its full context) is traced, so tests can assert what the model actually saw. `PI_FAMULUS_EVAL_KEEP=1` keeps sandboxes.

## Ablation (real models)

### Models and auth

Results of every run, as a scenario × model pass/fail matrix: [RESULTS.md](RESULTS.md). When they must be rerun: [BASELINES.md](BASELINES.md#when-to-rerun).

`eval/models.json` lists model specs exactly as `pi --model` takes them (`provider/id[:thinking]`); the first entry is the smoke model. Override per run with `--models a,b`.

Always spell out the thinking level (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`). Episodes isolate `PI_FAMULUS_HOME` but not pi's own config, so a spec without one runs at `defaultThinkingLevel` from `~/.pi/agent/settings.json`: results then depend on whose machine ran them. The spec, level included, is the model key in `results.jsonl`, so `x:low` and `x:high` are separate cells and can be compared in one report.

The eval never reads keys or `auth.json`. Each episode is the user's own `pi --model <spec>`, which resolves credentials (including OAuth refresh) the normal way. Models are validated against what pi can authenticate (RPC `get_available_models`) before anything runs. See what is available:

```bash
pi -ne --list-models
```

### What a run is made of

A run is a grid of **cells**, one per (model × variant × scenario), each repeated `k` times. By default (`--pairs affected`) a variant is paired only with the scenarios in its `affects`; `--pairs all` gives the full cross product. Every repeat is one **episode**: a fresh pi session given one scenario's task, graded PASS / FAIL / INVALID.

| Flag | Chooses | Default |
|---|---|---|
| `--tier smoke\|full` | A preset for the three flags below | `smoke` |
| `--models a,b` | Which models (`pi --model` specs) | smoke: first entry of `models.json`; full: all of it |
| `--variants a,b` | Which prompt texts to remove, one at a time. `baseline` removes nothing and always runs | smoke: `baseline` only; full: baseline + every ablatable segment in `manifest.json` + the groups |
| `--k N` | Repeats per cell | smoke: 3; full: 10 |
| `--scenarios a,b` | Which behaviors to probe (see [Scenarios](#scenarios-ablationscenariosts)) | the 10 default ones; compatibility probes are opt-in |

So **smoke** answers "does this model behave with the full prompt?", and **full** answers "which pieces of the prompt does that depend on?". An explicit flag overrides the tier's preset: `--tier full --models x` runs every variant on model `x` only.

Other flags: `--transcripts` (save each episode's event stream; see below), `--concurrency N` (episodes in parallel, default 3), `--pairs all` (run a variant on every scenario, not just the ones in its `affects`), `--max-episodes N`, `--results FILE`, `--keep` (keep sandboxes), `--judge <model>` (optional LLM judge for fuzzy criteria, recorded as `metrics.judge`, never overrides the programmatic grade).

### A complete first run

Run from `eval/`. Every command without `--yes` prints the plan and a cost bound and runs nothing: check it first.

```bash
npm run test:e2e                                        # free: harness and manifest are sound
pi -ne --list-models                                    # pick models pi can authenticate
node ablation/run.ts --tier full --models <a>,<b> --transcripts          # plan + cost
node ablation/run.ts --tier full --models <a>,<b> --transcripts --yes    # run it
node ablation/report.ts                                 # tables + verdicts
```

`--tier full` covers smoke: baselines run first, then the variants. For scale, all three models in `models.json` came to 252 cells / ≤2520 episodes, ~$169 at list price (2026-09-29, before early stopping).

Use `--transcripts` on a first run. Without it you get the grade and metrics (`polls: 12`) but not what the model did; with it you can read every tool call behind a FAIL. It costs disk, not model calls.

### Where results go

Everything lands under `eval/results/` (gitignored):

- `results/results.jsonl`: one line per episode (model, variant, scenario, pass, reason, metrics, usage). `report.ts` reads this.
- `results/transcripts/<label>.jsonl`: the event stream per episode, with `--transcripts`. Each result line's `transcriptPath` points at its file.

The runner resumes from `results.jsonl`: a cell with k scored episodes is skipped. That is also why two versions of the extension need separate files, or the second run skips everything. The name transition changes model-visible prompt/wake text, so use independent `--results` files; do not resume or mix episodes from a previous prompt version.

`PI_FAMULUS_EVAL_EXTENSION` swaps only the extension, not the manager, wake adapter, manifest, or ablation hooks. Use it only for post-transition revisions compatible with all of those. A pre-transition baseline must run with **its entire matching eval harness and manager**, not this harness with an older extension. No runtime compatibility aliases are provided.

For a cross-transition comparison, start in the current checkout's `eval/` with no extension, manager, or home environment overrides set. Use a fresh results directory for each comparison, and the same explicit model/thinking-level spec for both revisions:

```bash
(
  set -euo pipefail
  # Replace these values: baseline branch/SHA and provider/model:thinking-level.
  revision="<rev>"
  model="<a>"
  current_eval="$PWD"
  repo_root=$(git -C .. rev-parse --show-toplevel)
  mkdir -p "$current_eval/results"
  results_dir=$(mktemp -d "$current_eval/results/transition-comparison.XXXXXX")
  printf 'Results: %s\n' "$results_dir"
  worktree_dir=$(mktemp -d "${TMPDIR:-/tmp}/pi-famulus-baseline.XXXXXX")
  baseline="$worktree_dir/base"
  worktree_added=false
  cleanup() {
    status=$?
    trap - EXIT
    if "$worktree_added" && ! git -C "$repo_root" worktree remove "$baseline"; then
      printf 'Worktree retained at %s; inspect it before removing manually (no --force).\n' "$baseline" >&2
    else
      rmdir "$worktree_dir" || printf 'Temporary directory retained: %s\n' "$worktree_dir" >&2
    fi
    exit "$status"
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  git -C "$repo_root" worktree add --detach "$baseline" "$revision"
  worktree_added=true

  # Build and run entirely from the baseline revision, including its own harness.
  cd "$baseline"
  # Older revisions have no `deps` script: call nub directly.
  (cd extension && nub install --frozen-lockfile --node-linker hoisted)
  (cd eval && nub install --frozen-lockfile --node-linker hoisted)
  cargo build --release --manifest-path manager/Cargo.toml --target-dir eval/.cache/target
  cd eval
  node ablation/run.ts --tier full --models "$model" --transcripts --results "$results_dir/base/results.jsonl" --yes
  node ablation/report.ts --results "$results_dir/base/results.jsonl"

  # Candidate setup/run happens only if every baseline command succeeded.
  cd "$current_eval"
  npm run deps --prefix ../extension
  npm run deps
  node ablation/run.ts --tier full --models "$model" --transcripts --results "$results_dir/current/results.jsonl" --yes
  node ablation/report.ts --results "$results_dir/current/results.jsonl"
)
```

The single fail-fast subshell stops before the candidate if baseline setup, evaluation, or reporting fails. Each invocation creates a disposable detached worktree (leaving existing worktrees untouched) and a unique results directory, so old episodes/reports cannot be reused accidentally. Cleanup never uses `--force`: if the worktree has untracked files or changes, it is intentionally retained with a warning for inspection and manual removal; results are always kept.

The pre-transition `origin/main` baseline ignores `extension/node_modules/` in the root `.gitignore`, and `eval/node_modules/`, `eval/.cache/`, and `eval/results/` in `eval/.gitignore`. Other revisions use their own ignore rules: inspect them before building. After preserving any changes and cleaning only verified generated files, run `git worktree remove /retained/path/from/warning` from the current repository. If the worktree directory was removed outside Git, inspect stale registrations with `git worktree prune --dry-run`, then prune them. Pruning does not remove an existing retained worktree or replace inspecting its files.

Each revision's harness builds/selects its own manager and isolates each episode's manager home. Both result files and their transcript directories remain independent, under the absolute output directory above. Check the baseline revision's README for supported flags and prerequisites; compare only model/scenario cells supported by both revisions. Omit `--yes` from each runner command to inspect its plan before paying for episodes.

### Cost and early stopping

**Cost.** Without `--yes` the runner only prints the plan and a list-price upper bound per model (from pi's model catalog, assuming ~2.5k fresh + 6k cached input and ~450 output tokens per call). Early stopping usually cuts the full tier well below the bound; OAuth/subscription providers may bill nothing. The report shows the parent-session cost pi reported; child-session usage is not included.

**Early stopping.** Baselines run first. A variant stops as soon as its 95% Wilson interval is entirely below or above `baseline − 20pp`. Variants of a (model, scenario) whose baseline is below 20% are skipped ("floor"): no drop of 20pp is possible there.

### Scenarios (`scenarios/*.md`, graders in `ablation/scenarios.ts`)

Each scenario's prompt and what it tests live in `scenarios/<id>.md` (same format as `extension/prompts/`); the fixture and grader stay in code.

| id | passes when the model… |
|---|---|
| `bg-end-turn` | ends its turn after a command is backgrounded (no polling) and answers from the wake |
| `wake-continue` | acts on a task wake (writes the derived result), not just acknowledges it |
| `still-running-continue` | continues from one task's wake while another still runs |
| `handover-continue` | continues from a per-child `subagent-handover` before the run finishes |
| `monitor-not-sleep` | waits event-driven (monitor, or a backgrounded `tail -F … \| grep -m1`), never a sleep/poll loop |
| `no-fabrication` | never states the result before the wake (canary generated at run time) |
| `supervisor-reply` | answers a `supervisor-request` with `agent_message` `reply` |
| `resume-finished` | resumes a finished child via `subagent({action:"resume"})` (an `agent_message` attempt first is recorded, still a pass) |
| `overrun-stuck` | interrupts a child that is past its `timeout_ms` and blocked on a silent shell (`subagent-overrun` wake, reminders every 20s); extending it, steering it only, or ignoring two reminders FAILs |
| `overrun-progressing` | lets a child that is past its `timeout_ms` but printing progress finish (no action or `extend`), then writes its result; interrupting it FAILs |

Each grade is PASS / FAIL / INVALID (setup precondition not met, e.g. the command finished before the budget). Invalid and errored episodes are excluded from rates.

### Opt-in monitor / UI-waiter compatibility

These scenarios load an additional **safe captured `@injaneity/pi-computer-use@0.5.1` tool fixture**, never the production extension. All UI execution is stubbed; monitor processes run only generated local fixtures. This is a functional fixture, not an OS security sandbox. Default smoke **and full** grids exclude them, unless named explicitly with `--scenarios`. Existing scenarios never load the compatible tools/history.

| id | purpose |
|---|---|
| `monitor-waiter-event` | short ordinary/repeated monitor notifications; report tokens without a bogus UI wait (idle-turn behavior is prompted, not graded) |
| `monitor-waiter-rearm` | exactly two starts of the same fixture command: real 1s initial monitor timeout, re-arm once with 12s timeout, then repeated event wakes; no bogus UI wait |
| `monitor-waiter-ui-control` | positive: observe UI, use its state + meaningful predicate, report token revealed only by successful successor state |
| `monitor-waiter-synthetic-long` | repeated monitor wakes after incident-shaped generated history with four bogus historic waiter examples; target ~173k estimated tokens, **not incident-exact replay** |

**One new bad/no-op `wait_for` call fails**, even if schema validation/the executor errors or the eventual reply is normal. Graders check missing/invalid predicates and timeout schema bounds, fabricated/unobserved states/conditions, and monitor-source misuse. They do not treat the waiter name as global polling; genuine observed UI waits remain allowed, and the positive control fails if the model avoids the waiter entirely. Historic calls are not counted as newly issued calls. A proved compatibility misuse (`badWaiters > 0`) remains a scored FAIL if an ancillary provider/quiet-window error follows: its provenance is retained in `metrics.episodeError`, not the framework's top-level ERR flag. All other scenario/error policies are unchanged. Missing fixture/tool loading is INVALID, not a placebo pass.

Ordinary/long probes require exactly one successful monitor start; duplicate monitor stacking fails. The re-arm probe associates the timeout with the initial task id and checks both command/timeout configurations and ordering.

Metrics expose fixture/computer-use versions, history and request-context sizes, active/all tool counts and bounded active tool names, model/thinking spec and resolved provider/id/API, runtime pi/Node versions, separately labeled eval development SDK version, and captured source/loaded schema hashes. Serialized chars/4 is only an estimate; actual provider usage is authoritative. The long-case cost estimate charges 220k extra **uncached** tokens per model call; actual costs/tokenization may differ. See [fixture provenance, safety, and evidence limits](ablation/fixtures/README.md). Short probes cannot claim to reproduce the large-context incident. The deterministic compatibility tests were run in CI for commit `ed9d571`; changes in this review-fix commit await CI. No local eval tests or real-model eval/probes were run.

Commands for a user-authorized later run (from `eval/`; replace the explicit model/thinking spec):

```bash
# Deterministic tests first; no model calls.
node --test ablation/monitor-waiter.test.ts

# Plan short negative + positive controls. Add --yes only when ready to run.
node ablation/run.ts --models <provider/model:thinking> --scenarios monitor-waiter-event,monitor-waiter-rearm,monitor-waiter-ui-control --variants baseline,guidelines.monitor-end-turn --k 3 --concurrency 1 --transcripts --results results/monitor-waiter-v1/short.jsonl

# Expensive synthetic-long is selected separately and explicitly; inspect cost first.
node ablation/run.ts --models <provider/model:thinking> --scenarios monitor-waiter-synthetic-long --variants baseline,guidelines.monitor-end-turn --k 3 --concurrency 1 --transcripts --keep --results results/monitor-waiter-v1/long.jsonl
```

Use fresh result filenames after fixture/prompt changes; do not mix revisions or resume old cells. `--transcripts` records live events, not the request-local history prefix; `--keep` retains the generated history/context audit. `guidelines.monitor-end-turn` is the span in `extension/prompts/guidelines.md` (`# monitor-idle`) and removes **all copies** (system guidelines, tool description/rules, start result). The separate `result.monitor-started-instruction` removes only the first two start-notice sentences, avoiding overlap with the idle-instruction segment.

### Reading the report

Per model, a variant × scenario matrix: `pass% (passes/n) [95% Wilson CI]`, and for variants `Δ` vs baseline in percentage points. `▼` marks a drop ≥ 20pp. `(k vacuous)` means the removed text never appeared in k of those episodes (surface not reached), so they equal baseline.

Segment verdicts:

- **load-bearing** — removing it dropped the pass rate by ≥ 20pp on at least one (model, scenario).
- **slop** — tested and never load-bearing. Candidate for deletion (check the CI width first: with small n a real effect can hide).
- **untested** — no scored cell (not in any `affects`, or floor/no baseline).
- **not ablatable** — only child sessions see it (see below).

`rules.pi-env` is a **control**: if it ever comes out load-bearing, the differences are noise and the run needs more k.

### What gets ablated, and how (`harness/ablation-ext.ts`, `ablation/manifest.json`)

The ablation harness is a separate pi extension loaded after ours; the extension itself is never edited.

- `before_agent_start` edits `systemPromptOptions` in place (the `pi-famulus` section, `<rules>` guidelines) for user-turn initialization. These edits alone do not persist through every wake/tool-result boundary: the core may clear them.
- Famulus's request-local `context_with_system` repair maintains its guideline visibility on ongoing/wake requests. The ablation `context_with_system` hook, registered last, then strips segments from **every request**: system guidelines, tool declarations, tool results, and wakes (the shared `FAMULUS_WAKE_LEAD_IN`, `<reply-with>`). Non-destructive: the session keeps the original text.
- `tool_call` disables the bare-sleep guard (`mech.sleep-block`); `mech.autobg` is disabled via `config.json`.

Segment texts are not copied here: a segment is the span marked `<!--seg:<id>-->…<!--/seg-->` in [`extension/prompts/`](../extension/prompts/INDEX.md), the same files the extension is built from. Only structural spans that are no prompt (the `<reply-with>` element) keep a `pattern` in the manifest. `npm run test:e2e` checks that every span reaches the model verbatim in a real pi session, that every removal happened, and that no removed text is still visible.

**Not ablatable externally:** anything only child sessions see (`child.guidelines` = `CHILD_BEHAVIOR_GUIDELINES`, child tool descriptions). By design, children load no extensions, so no hook runs there. These segments are drift-guarded in child calls but never scheduled. Wake delivery itself (triggerTurn vs steer routing, coalescing) has no interception hook either.

## Layout

```
lib/        rpc.ts (pi RPC driver) · sandbox.ts · transcript.ts · wake-adapter.ts (only module that parses <pi-famulus-wake>) · models.ts · paths.ts
harness/    faux-ext.ts (scripted model) · ablation-ext.ts
e2e/        faux.test.ts · ablation-harness.test.ts · tui.test.ts · faux-dsl.ts · scripts/
ablation/   manifest.json · scenarios.ts · graders.ts · episode.ts · run.ts · report.ts · stats.ts · judge.ts · fixtures/
```

When the wake format changes, update `lib/wake-adapter.ts` (and the manifest if prompt text moved); graders consume the normalized `Wake` and should not change.
