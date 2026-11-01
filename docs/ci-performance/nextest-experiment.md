# Controlled nextest cost experiment

The previous CI change reduced wall time, but its final run still consumed
**125m55s of summed runner-active time**. This experiment evaluates compute
savings, not just additional job parallelism. It does not replace normal CI
or alter the six-platform release/packaging matrix.

## Boundaries

- Branch: `perf/nextest-cost`; measured commit: `9231f2dfc28591b957347913c016babcec084544`.
- The completed workflow is archived at `.github/experiments/nextest-cost.yml`
  to prevent further report-only pushes from repeating this expensive experiment.
- Ubuntu x64 and Intel macOS, each with real and manual clocks: four jobs.
- Prebuilt, version-pinned **cargo-nextest 0.9.150**, installed by a SHA-pinned
  action. No `cargo install`, sccache change, or library refactor.
- Existing dependency cache configuration is reused read-only. Experiments
  do not create additional repository caches.
- Windows keeps Cargo. The custom harness's MSYS keeper assumes one harness
  process; nextest's process-per-test model needs separate Windows validation.

## Coverage and scheduling

`scripts/ci-nextest-benchmark.py` compiles once and reads Cargo's actual
compiler-artifact records. It queries each executable's ordinary and ignored
lists, then reconciles exact target names, test names and ignored flags with
nextest JSON. New targets are included automatically; unsupported target kinds
fail closed. The custom platform inventory must be nonempty, and the debug
resource benchmark must remain disabled.

The candidate is a **disjoint partition**, not a filtered-down test suite:

| Runner | Targets |
| --- | --- |
| Original Cargo harness | `backpressure`, `lifecycle_adversarial`, `upgrade`, `timing_canary`, `platform`, `resource_bench` |
| nextest | All other compiled test targets, including binary unit tests |

The two phases run sequentially. Cargo retains its original internal test
parallelism; no `--test-threads=1` is introduced. This preserves C6/D2 isolation,
upgrade U11's documented dependence on concurrent suite stress, timing-canary
isolation, and the custom harness execution model. The separate opt-in release
resource benchmark remains unchanged in normal CI.

Both runners use no fail-fast; nextest has **zero retries** and fails on an empty
selection. A failed nextest phase does not prevent the Cargo phase or subsequent
comparison runs from executing. Any failed run makes the experiment fail.
Existing ignored tests remain ignored, with their names and flags audited.

The compatibility patch also makes the custom harness consume `--format terse`
and report an empty ignored inventory correctly. The disabled resource benchmark
exposes an empty registry rather than non-protocol discovery output. CLI tests
exercise the real harness through a tiny rustc-built fixture; they do not build a
second copy of the full Cargo project.

## Measurement

Each job runs **Cargo → candidate → candidate → Cargo** on the same runner,
checkout, features, profile and compiled artifacts. Compilation and inventory
validation are timed or logged separately. The candidate measurement includes
both its nextest and retained-Cargo phases. Full logs, exact inventories, commands,
exit codes, compiler/tool versions and runner-image metadata are uploaded.

AB/BA reduces order bias but is only two observations per runner/clock lane, not
a statistically large sample. Local runs are functional evidence, not a proxy
for GitHub-hosted performance.

Do not equate the script's test-command durations with full CI savings:

1. Report every failure, ignored-test count and inventory mismatch.
2. Compare paired execution times, including both candidate phases.
3. Account for nextest installation and any candidate-only discovery/orchestration
   work using job/step durations. Benchmark-only comparison overhead is reported
   separately, not presented as production overhead or savings.
4. Only expand after a reliable measured gain; preserve all other CI checks.
5. A promoted change still needs a complete remote CI run and a new summed
   runner-active total before claiming improvement over **125m55s**.

## Initial local result (not a successful performance comparison)

Real-clock discovery reconciled **20 targets / 868 tests / 2 ignored**. Counts
are platform- and feature-specific, not a hardcoded cross-platform baseline.

| Local arm | Command time | Outcome |
| --- | ---: | --- |
| Full Cargo | 265.232s | 861 passed, 5 failed, 2 ignored |
| nextest partition | 318.407s | 746 passed, 6 failed, 1 ignored |
| Retained Cargo phase | 96.507s | 114 passed, 0 failed, 1 ignored |
| Combined candidate | 414.914s | Failed |

The baseline failed five observability cases; the candidate failed five
observability cases and one intercom case. No leak warnings were found. Both
arms completed all 866 nonignored tests, but **neither arm passed**, so these
numbers establish neither a successful speedup nor GitHub-hosted performance.
The experimental script correctly exited nonzero. Isolated diagnostic passes
are not counted as fixes or successful replacement runs.

The candidate's trivial unit tests took seconds each on this developer machine,
versus 12.38s for all 427 nonignored unit tests under Cargo. A bounded diagnostic
of the same compiled hermetic-environment test measured median startup of
**0.214s with the developer PATH versus 0.0169s with a minimal system PATH**
(three alternating samples each). The developer PATH required nine mirror
directories / 3,883 symlinks per process; the minimal PATH required none.
Hermetic re-execution remained enabled in both diagnostics. This establishes
substantial developer-machine startup overhead, not the root cause of the
original test failures or proof of GitHub speedup. The experiment records PATH
mirror counts on hosted runners without changing their test environment.

## Remote result and decision

[Run 41328307787](https://github.com/Leechael/pi-famulus/actions/runs/41328307787)
completed successfully: **4/4 jobs and all 16 full-suite arms passed**, with no
retries, leak warnings or inventory mismatches. The experiment itself consumed
**98m29s summed runner-active time**, with **31m57s wall time**. This is one-off
measurement cost, not the cost of a proposed production pipeline.

The table uses the mean of each arm's two measurements; the candidate includes
both its nextest and retained-Cargo phases. Individual observations are preserved
in [`nextest-abba.csv`](nextest-abba.csv).

| Lane | Cargo mean | Candidate mean | Saved | Test-command reduction |
| --- | ---: | ---: | ---: | ---: |
| Linux x64, real | 252.25s | 229.01s | 23.24s | 9.2% |
| Linux x64, manual | 257.47s | 247.86s | 9.61s | 3.7% |
| Intel macOS, real | 405.61s | 366.20s | 39.41s | 9.7% |
| Intel macOS, manual | 383.71s | 362.18s | 21.53s | 5.6% |
| **Sum of four lane means** | **1299.04s** | **1205.25s** | **93.79s** | **7.2%** |

The installer steps total **3 seconds at GitHub's whole-second resolution**.
Subtracting those gives about **91 seconds potential savings across these four
lanes**, before any additional production orchestration cost. That is only about
**1.2% of the previous 125m55s complete-CI total**. It is **not** a measured new
complete-CI total: this experiment deliberately reused one compilation for both
arms, and both arms include the new harness regression target. No six-platform
or whole-pipeline extrapolation is claimed.

Coverage by lane:

| Lane | Targets | Discovered tests | Existing ignored | Completed nonignored per arm |
| --- | ---: | ---: | ---: | ---: |
| Linux x64, real | 20 | 874 | 2 | 872 |
| Linux x64, manual | 20 | 897 | 2 | 895 |
| Intel macOS, real | 20 | 868 | 2 | 866 |
| Intel macOS, manual | 20 | 891 | 2 | 889 |

Every candidate nonignored name has exactly one successful runner outcome in
both observations. Baseline names all appear as successful too; on Linux the
native-alias cancellation test deliberately re-executes a libtest child, which
prints an additional nested result for the same name. This is existing test
implementation behavior, not a runner retry.

All hosted runners reported **zero PATH mirror directories**, unlike the local
developer machine. They used Rust **1.99.1** and four logical CPUs. Intel macOS
showed appreciable within-job variation (for example, its real-clock Cargo runs
were 432.27s and 378.95s); two observations are not a tight confidence interval.
Full compile/discovery logs, tool/image metadata, exact inventories and command
exit codes are attached to the Actions run. Cache usage remained unchanged at
**9,226,810,614 bytes / 8.593 GiB**, with **40 cache entries**.

**Decision: do not expand or promote this experiment now.** The measured
Unix x64 hybrid is compatible with the audited coverage and modestly faster,
but it is not the main answer to the 125-minute total. Windows and ARM adoption
remain unvalidated. Normal CI, PR #103, release validation and main are unchanged.
The next separate investigation should target compilation structure and actual
cacheability (thin binary/library boundary and Git-SHA invalidation), rather
than claiming that adding sccache alone will cache the current binary crate.

To deliberately repeat the experiment, restore the archived YAML under
`.github/workflows/nextest-experiment.yml` on `perf/nextest-cost`, inspect its
branch trigger and pinned tool versions, then push. It is not left automatically
running on documentation updates.
