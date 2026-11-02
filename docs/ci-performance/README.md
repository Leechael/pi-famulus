# GitHub Actions performance audit

Measured 2026-11-01 for `Leechael/pi-famulus`. This directory records the baseline and live branch experiments; it is not a claim that hosted-runner latency is deterministic.

## Baseline: the latest 50 runs

The frozen sample spans 2026-10-28 02:41:34 UTC to 2026-11-01 09:21:19 UTC:

- 20 CI, 25 Upstream drift, 5 Publish npm packages runs.
- 41 successful, 5 failed, 4 cancelled runs.
- All pages and attempts: **718 job executions, 10,396 steps**. `jobs-before.csv` contains every job, including failures, cancellations and reruns.
- Clean successful CI with one attempt (n=11): **27m56s median**, **37m51s p90** wall latency; **128m46s median**, **135m15s p90** summed runner-active time. Parallel runner time is not wall time or billing-weighted minutes.
- Two successful CI runs contained three fully successful attempts each. Their 76–84 minute overall durations are not evidence of runner queueing or flaky tests.
- Darwin x64 was the last check in 12 of 13 successful CI runs.

The most recent main run, [41180558908](https://github.com/Leechael/pi-famulus/actions/runs/41180558908), took **20m55s** wall / **120m12s** summed runner-active time. The most recent PR run, [41188561946](https://github.com/Leechael/pi-famulus/actions/runs/41188561946), took **20m58s** / **122m01s**. These recent runs are important comparators alongside the broader median; using only the slower historical median would exaggerate the improvement.

### Native job breakdown

Successful single-attempt CI, n=11 per row. Seconds; medians of components need not sum to median total.

| Host | Start-delay proxy median / p90 | Active median / p90 | Preparation + compilation median | Checks/test interval median |
|---|---:|---:|---:|---:|
| darwin-x64 | 6 / 166 | 1540 / 1764 | 692.8 | 839.8 |
| darwin-arm64 | 7 / 304 | 1252 / 1302 | 342.5 | 899.7 |
| linux-x64 | 3 / 192 | 881 / 911 | 279.2 | 597.5 |
| linux-arm64 | 6 / 55 | 867 / 874 | 239.3 | 622.4 |
| win32-x64 | 3 / 125 | 740 / 760 | 457.9 | 273.5 |
| win32-arm64 | 4 / 106 | 779 / 842 | 367.9 | 402.9 |

Latest PR darwin-x64 step/log analysis:

| Step | Wall | Preparation/compilation | Actual test interval |
|---|---:|---:|---:|
| Real-clock Cargo suite | 442s | ~84s | ~358s |
| Manual-clock Cargo suite | 429s | ~72s | ~357s |
| Release resource benchmark | 175s | ~132s | ~43s |
| Explicit-target release build | 127s | 127s | — |

The bottleneck is **both serialized testing and duplicated release compilation**, not just checkout or dependency installation. `intercom` alone took 129/153 seconds in the two clock modes. Clippy was comparatively small: successful effective-job medians of 44/72/89 seconds on Linux/macOS/Windows.

### Timing definitions and limitations

`workflow_created_to_job_start_s` is a **start-delay/queue proxy**. It includes dependency waits, capacity, concurrency, scheduling and earlier-attempt/human rerun delay. GitHub's REST job data does not expose the moment a runnable job actually entered the runner queue. `attempt_started_to_job_start_s` removes earlier-attempt elapsed time, not dependency waits. Neither is pure runner queue latency.

Each CSV row accounts for active time as:

```text
active = preparation_build + checks_tests + artifact_delivery_publication
       + teardown + unresolved_mixed + unattributed_active
```

Cargo log timestamps separate compilation from the first `Running unittests` / `Running tests/...` boundary. Other checks, including package-install smoke scripts, remain command-level intervals when their internal setup cannot be separately timed. Post-actions/cache saves are teardown; gaps remain unattributed. Skipped jobs have zero active time. One cancelled native log was inaccessible and one cancelled 11-second Cargo interval remains unresolved; successful baseline native jobs have no unresolved Cargo intervals. p90 uses linear interpolation.

The baseline is observational across different commits, runner images and cache states, not a randomized same-SHA experiment.

## Changes and coverage contract

```text
scope + compatibility matrix
  ├── manager tests: six platforms × real/manual clock, in parallel
  ├── native release benchmark + pack/install smoke: six platforms
  ├── platform-specific clippy: Linux/macOS/Windows, debug + release
  ├── extension compatibility + Windows suites
  └── real-manager integration + eval/E2E: all original hosts
```

- Split the **two complete** Cargo suites into separate clock lanes, with no test-name filters or omitted test binaries. Package builds have no dependency on debug tests. Their results still gate the reusable workflow and publication.
- On macOS/Windows, run release benchmarks in the same explicit target directory as packaging. Keep Linux host/glibc benchmarks **and** musl/static package validation.
- Run the identical locked-source compatibility typecheck once, while retaining typechecks after **every** selected pi-version install. Run the OS-independent eval TypeScript check once on Ubuntu, without adding another job.
- Retain all original grader and eval runtime tests. In particular, grader tests exercise BSD `tail` and real process-group cleanup on macOS; clippy checks platform `cfg` branches and release-only code. Moving those entire checks to Ubuntu would reduce coverage.
- Only PR runs share a cancellable concurrency group. Main, dispatch and schedule runs have a unique run-ID group; `cancel-in-progress: false` alone would still allow GitHub to replace an older pending run in a shared group.
- Use job-level path filtering rather than suppressing the required-check workflow. Pure documentation runs retain README synchronization/link checks and the historical `extension` completion check. Classification errors fail closed.
- The reviewed docs allowlist excludes executable Markdown such as `extension/prompts/**` and `eval/scenarios/**`.
- No branch protection/rulesets were observed on main via API (classic protection 404, effective/inherited rules empty). No repository protections were changed.
- No `cargo install`/`go install` tool bootstrap exists in these workflows, and no Docker build participates. No replacement tool installation was necessary.

### Cache budget

Initial usage endpoint: **7,678,455,554 bytes**, 30 entries. Separately paginated inventory: 31 entries / 7.151 GiB (non-atomic API snapshots). Rust accounted for 4.592 GiB; npm 2.559 GiB. The six main native caches alone used 2.837 GiB.

Manager clock lanes share a host cache with one writer, rather than duplicating caches per feature lane. Dependency caches are saved even after a test failure, so fixing a regression does not require another cold dependency build. `test-clock` only enables this crate's feature, not additional dependency features. `CARGO_PROFILE_DEV_DEBUG=line-tables-only` retains line-number backtraces without large variable/type debug sections. Workspace crate outputs are not opted into the dependency cache. Native package jobs now enable `setup-node`'s npm download cache. Their install smoke tests capture that cache path before isolating `HOME`; a regression test reproduces the old throwaway sandbox-cache behavior and verifies reuse. Jobs that do not install npm dependencies do not create one. No `node_modules` or packaging artifacts are stored as dependency caches.

## Local incremental build verification

The checkout already had both Git metadata files, so its original two consecutive builds were **64.47s → 0.374s**. After the fix they were **45.14s → 0.362s**. These timings demonstrate a fresh second build, **not** a measured 64→45 second speedup attributable to the fix.

A tiny real Git/Cargo fixture reproduced the actual defect: watching nonexistent `.git/packed-refs` caused Cargo to report `StaleItem(MissingFile ...)` on every invocation. `manager/build.rs` now watches existing packed refs and the loose ref or nearest existing parent. Three regression tests verify fresh second builds, ref packing and updates, detached HEAD, linked worktrees and source archives. They failed before and pass after the fix.

## Live experiments

The experiment branch is `perf/actions-pipeline`, [PR #103](https://github.com/Leechael/pi-famulus/pull/103). Final successful/warm-run timings and links are recorded in that PR's description, avoiding another full code-PR rerun solely to commit its own timing result. No production publication or main merge is part of this experiment.

### Round 1: cold cache / failed regression, not a speedup claim

[Run 41316249795](https://github.com/Leechael/pi-famulus/actions/runs/41316249795) took **16m36s** wall / **131m24s** summed runner-active time. It failed: all eight Unix clock lanes caught the newly added build regression fixture using a bare process constructor rather than the repository's descriptor-safe, hermetic harness. The fix uses `common::command` and explicit Cargo toolchain binaries within the hermetic environment. No guard or test was disabled. The four Windows clock lanes, all six package jobs and all existing E2E/compatibility/clippy checks passed. A failed run that stops test binaries early is **not comparable to a complete green baseline**.

All 18 new Rust job instances missed cache. Non-Linux package `Build release binary` steps fell to **0–1s** because they reused the benchmark build; Linux's distinct musl builds remained 73/85s. The macOS x64 package job spent 436s compiling cold release dependencies, while its tests took 45s. Observed macOS concurrency peaked at exactly five; x64 package/test jobs waited for ARM package/E2E slots. This confirms that adding more macOS shards is counterproductive without more capacity.

Cache inventory after this run was 9,630,137,237 bytes / 43 entries. Six caches belonging exclusively to already-merged PRs #94/#101 were verified and removed, reclaiming **1,213,281,958 bytes**. No main or active PR #103 cache was deleted. Four Unix manager caches had not yet been saved because those tests failed; a fixed green run is required before a fully warm comparison.

### Round 2: complete manager/package coverage, Windows readiness failure

[Run 41318188194](https://github.com/Leechael/pi-famulus/actions/runs/41318188194) took **17m04s**, with **132m28s** summed job-active time. All 12 complete manager clock lanes and all six package jobs passed. The existing Windows E2E observer test hit its unchanged 8-second readiness deadline; the new aggregate correctly failed the workflow rather than accepting partial success. This is another **failed-run diagnostic**, not a successful speedup measurement.

The observer test and implementation are unchanged from main, and no timeout, skip or automatic retry was added. Its prior run passed, but the Windows runner image and Node patch version changed between runs (`20261021.278.1` / Node `24.22.1` → `20261030.294.1` / `24.22.2`). Startup/scheduling sensitivity is a hypothesis, not a proven root cause or an established historical flake. The final full run is required to verify all checks together.

All four Unix manager cache families were still cold during round 2; afterward all six host cache families existed. Post-run cache inventory was **9,226,810,614 bytes / 8.593 GiB**, leaving **1.407 GiB** below the 10 GiB budget. The third run is the first fully cache-warmed experiment; it also removes the redundant portable typechecks described above.

### Live docs-only / cancellation probe

A separate draft [PR #104](https://github.com/Leechael/pi-famulus/pull/104), based on the optimization branch, changed only `docs/ci-scope-probe.md`. A second push superseded queued CI/upstream runs:

- Old CI [41316403040](https://github.com/Leechael/pi-famulus/actions/runs/41316403040) and upstream [41316402921](https://github.com/Leechael/pi-famulus/actions/runs/41316402921): cancelled.
- Latest CI [41316431591](https://github.com/Leechael/pi-famulus/actions/runs/41316431591): success, 124s wall, only **6s scope + 4s aggregate** active. All heavy families skipped; README checks 2/2 passed.
- Latest upstream [41316431512](https://github.com/Leechael/pi-famulus/actions/runs/41316431512): success, 111s wall, **5s active**, drift matrix skipped.

This proves queued supersession, not interruption of a running heavy test. The probe also exposed `always()` keeping an obsolete aggregate alive after cancellation; round 2 changes that condition to `!cancelled()`. The stable historical `extension` gate now checks **all** CI families, not just compatibility. Tests execute the actual inline gate against every failed/cancelled/missing/skipped dependency combination.

A final-version repeat in [PR #105](https://github.com/Leechael/pi-famulus/pull/105) verified both newest workflows succeed and superseded workflows cancel: [CI 41318405122](https://github.com/Leechael/pi-famulus/actions/runs/41318405122) took **50s wall / 9s active**; [upstream 41318405094](https://github.com/Leechael/pi-famulus/actions/runs/41318405094) took **46s wall / 10s active**. Its old aggregate was reported as `cancelled` with **no runner and no steps**, not literally `skipped`; it no longer kept the old workflow alive. The temporary PR/remote branch/worktree were cleaned up without merging.

Skipped matrices report literal names such as `clippy (${{ matrix.os }})`, not every expanded platform name. Branch protection should require the stable **`extension`** aggregate rather than a conditional matrix name. PR #104 was closed without merging and its remote branch/worktree removed.

## Round 4: overlap whole Cargo test binaries on Unix

After round 3 the critical path was still a macOS manager clock lane, and the test interval dominated it: `cargo test` runs its ~19 test binaries **one after another**, and `intercom` alone took 100–180s. The suite is wait-bound, not CPU-bound. A local 4-core Linux run of the complete real-clock suite took **6m03s wall for 1m22s user CPU**. `intercom` used **~8s of CPU over 80–104s** at 8 or 16 libtest threads.

`scripts/cargo-test-parallel.mjs` keeps coverage identical and changes only the scheduling:

- One `cargo test --no-run --message-format=json` with the lane's exact arguments builds every test target. Each executable cargo reports is run from the package root with no filters or skips. There are no doctests because the crate has no library target. Any non-zero exit, signal or spawn error fails the step.
- Binaries start longest-first, at most `min(4, max(2, cores))` at a time. `intercom` gets `--test-threads=8`, unless `RUST_TEST_THREADS` is set explicitly.
- The real-time `timing_canary` runs **alone afterwards**, so its wall-clock bounds never compete with other suites.
- Each binary's output is printed whole when it finishes: collapsed on success and expanded on failure. A per-binary duration table goes to the log and the job summary.
- Windows keeps plain `cargo test`: its suites take ~77s, and Git Bash start-up is racy under concurrent first launches (`tests/platform.rs`).

Local A/B on the same build and the same 4-core container (seconds of wall time):

| Lane | `cargo test` | Parallel driver | Parallel driver, intercom ×8 threads |
|---|---:|---:|---:|
| real clock | 363 | 185 | **113 / 112** (two runs) |
| manual clock (`--features test-clock`) | 361 | — | **119** |

CPU time was unchanged (user 1m20–1m27s in every run). The same pre-existing failures appeared with and without the driver. These were process-reaping assertions in `lifecycle_adversarial`/`platform`/`timing_canary`, and they fail in this sandbox container, which has no init subreaper, but pass on every CI host. The driver introduced no new failure.

### Considered and not adopted

- **cargo-nextest**: it needs a downloaded tool, and the custom `harness = false` binaries (`platform`, `resource_bench`) would need `--list --format terse`/`--ignored` support. It also runs one process per test, which changes the semantics of the shared MSYS keeper on Windows.
- **[mr-boxington](https://mr-boxington.jdx.dev/) (`mbx`)**: this is a shared, self-pruning local Cargo target cache. Its author notes that its remote cache does not yet consistently beat `Swatinem/rust-cache` on GitHub-hosted runners, and round 3 already had 25/25 exact Rust cache hits. The CI cost is compiling the workspace crate itself and running tests, not dependency restore. It is a reasonable developer-workstation tool, but not a CI change here.
