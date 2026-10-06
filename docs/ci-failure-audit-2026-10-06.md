# Actions failure audit — 2026-10-06

Snapshot window: 2026-10-04 11:02:44 UTC through 2026-10-06 11:02:44 UTC.
This is a fixed audit window, not a claim about subsequent runs.

## Inventory

Inspected all 37 runs, all 42 attempts, and 393 job executions, including
failed jobs inside cancelled runs and earlier attempts of successful reruns.
There were 11 failed jobs across eight runs, plus eight hosted-runner
acquisition failures recorded as cancelled jobs with failure annotations.

| Run / attempt | Platform / failed path | Disposition |
|---|---|---|
| [37439173913](https://github.com/Leechael/pi-famulus/actions/runs/37439173913) | Linux x64: C6 list latency 2.958s; D8b shutdown refusal | #40 fixes the shutdown handoff with regressions; C6 harness isolation is mitigation, not a confirmed latency root cause. |
| [37441645793](https://github.com/Leechael/pi-famulus/actions/runs/37441645793) | macOS ARM64: `signal_group_kills_whole_tree` EOF timeout | #40 repairs the unit fixture's fork/readiness race; production group signalling is unchanged. |
| [37376206040](https://github.com/Leechael/pi-famulus/actions/runs/37376206040) | macOS ARM64: same group-kill unit fixture | Same #40 regression coverage. |
| [37376282737](https://github.com/Leechael/pi-famulus/actions/runs/37376282737) | macOS integration teardown: `ENOTEMPTY` | #39 shuts the daemon down before removal and retries unlink. |
| [37362423489](https://github.com/Leechael/pi-famulus/actions/runs/37362423489) | macOS Intel: C6 producer exceeds 90s | Still unconfirmed. The daemon and producer were alive; no live output-size/CPU history was archived. Do not equate this with the 2.958s latency failure. |
| [37377535922](https://github.com/Leechael/pi-famulus/actions/runs/37377535922) (cancelled) | Linux ARM64, test-clock: U10 emits `a�a中b\n` | This follow-up fixes a confirmed tee/carry-snapshot race, with deterministic actual-pump and upgrade regressions. |
| [37334382441](https://github.com/Leechael/pi-famulus/actions/runs/37334382441), attempt 1 | macOS: cold restart `connect()` returns false; attempt 2 passes | This follow-up fixes reproduced TS startup failure classes. The exact historical cause is unknown because the error and integration home were discarded. |
| [37376950128](https://github.com/Leechael/pi-famulus/actions/runs/37376950128) (cancelled), three jobs | Windows extension path assertion; native/e2e runner compile errors | Unmerged Windows feature branch only; fixed there by `4b443ef`. Not main/release coverage. |
| [37377535922](https://github.com/Leechael/pi-famulus/actions/runs/37377535922) (cancelled), Windows job | Three Unix runner-usage assertions on Windows | Unmerged feature branch only; gated there by `eb71e0e`. This does not establish Windows CPU-accounting coverage. |

The eight infrastructure failures are in `37365821211` attempts 1–3 and
`37364182868` attempt 1: “The job was not acquired by Runner of type hosted
even after multiple attempts.” They ran no test steps. Other cancelled jobs
have explicit concurrency-supersession annotations, not unexplained test
failures. All three rerun runs (`37365821211`, `37334382441`, `37364182868`)
were inspected attempt by attempt.

## Follow-up evidence

- **UTF-8:** artifact `11373200252` contains correct disk bytes
  `61 e4 b8 ad 62 0a`, but incorrect watcher events `a�a` at cursor 1,
  then `中b\n` at cursor 6. Starting tee before taking the file carry
  snapshot lets the same initial bytes enter both carry and the chunk queue.
  The deterministic real-pump regression was red with the old ordering
  and green after snapshotting before tee creation. U10 now gates the
  continuation until after upgrade/reconnect and checks raw bytes and cursors.
- **Startup endpoint gap:** socket/pid removal does not mean `manager.lock`
  has been released. A real Rust daemon contender exits zero without creating
  a socket. The failure-SHA TS client still returned false after the lock was
  released; the corrected public client creates/reaches the successor.
  The integration regression uses an actual OS flock, actual Rust processes,
  and Node's process diagnostic channel, not simulated spawn results.
- **Shutdown probes:** exact refusal matching, bounded individual probes,
  preserved successful connections, permanent-error handling, and one startup
  deadline are covered separately. Rust also waits after an initial transport
  failure while the lifetime lock remains held.
- **Observability:** real-manager connection assertions include `lastError()`.
  CI preserves its isolated home before teardown, excludes sockets/symlinks
  and files larger than 1 MiB, and uploads it alongside eval sandboxes.

## Remaining uncertainty

C6's historical Intel stall is not proven fixed. A scratch-only experiment
reproduced the same producer deadline while bytes were still progressing by
limiting daemon scheduling to 10%. Skipping encoding for full watcher queues
plus a conservative UTF-8 full-chunk fast path turned that controlled case
green without changing the 256 MiB / 90s / 2s / 96 MiB canary. Those performance
changes are **not included in this follow-up**; the experiment does not identify
the historical runner's cause.

#40 merged `8605304` into `a79fd8d`; publish run
[37461690429](https://github.com/Leechael/pi-famulus/actions/runs/37461690429)
succeeded on that merge. The later UTF-8 fix (`c9b3c9e` in this PR, originally
`9b79647` before rebasing) and TS fixes were not part of that successful
publication. A green run is evidence of that run, not proof that every
intermittent failure has disappeared.
