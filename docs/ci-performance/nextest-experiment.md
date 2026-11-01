# Controlled nextest cost experiment

The previous CI change reduced wall time, but its final run still consumed
**125m55s of summed runner-active time**. This experiment evaluates compute
savings, not just additional job parallelism. It does not replace normal CI
or alter the six-platform release/packaging matrix.

## Boundaries

- Branch: `perf/nextest-cost`; workflow: `nextest-experiment.yml`.
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

Remote results remain pending. Normal CI has not been switched to nextest.
