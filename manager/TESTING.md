# pi-famulus testing

How the manager is tested, what the tests are known to guard, and where the
gaps are. Contract sources: `docs/design.md` §3 and `docs/cli.md`.

## Suites

| Suite | Kind | What it covers |
|---|---|---|
| `src/**` `#[cfg(test)]` | unit | ring buffer, record persistence, state mapping, daemon lock claim, UTF-8 chunk cutting, signal names, id format, manual clock (`test-clock` only) |
| `tests/protocol.rs` | black box | message round-trips, basic lifecycle (t01–t13) |
| `tests/lifecycle_adversarial.rs` | black box | every cell of the lifecycle table below, adversarial conditions |
| `tests/mutation_gaps.rs` | black box | behaviours found unguarded by cargo-mutants survivors (g1–g14) |
| `tests/observability.rs` | black box | observability contract: protocol additions, events.jsonl, inspection CLI (p1–p3, e1–e4, c1–c9) |
| `tests/upgrade.rs` | black box | in-place upgrade: exec handover, rollback, restore failure, carried watches, N−1 hello (u1–u12) |
| `tests/timing_canary.rs` | black box, real time | the actual 5s idle grace and 2s kill grace (always on the real clock) |
| `tests/cli.rs` | black box | CLI help/version, completion scripts, strict parser errors (no daemon / no `--home`) |
| `tests/common/mod.rs` | helpers | wire client, isolated `--home`, process probes, crashable helper client, clock stepping (`Home::advance*`) |

Daemon-facing black-box tests (`protocol`, `lifecycle_adversarial`,
`mutation_gaps`, `observability`, `upgrade`, `timing_canary`) start the
compiled binary with an isolated `--home` (`$TMPDIR/pi-famulus-test-<pid>-<test>`,
kept short for the ~104-byte socket path limit) and speak the u32-BE +
JSON protocol directly. They depend only on `serde_json` and `libc`,
which are already regular dependencies, so there are **no new
dev-dependencies**. `tests/cli.rs` is different: it only drives argv
parse/help/completion paths and never opens a `--home` or a socket.

Determinism rules:

- Every wait is a poll with a deadline (`poll_until`). The daemon's own
  timers are stepped, not waited for: see [Time in tests](#time-in-tests).
- A crashing pi is a real separate OS process: the test binary re-executes
  itself as `helper_hold_extension_conn` (an ignored no-op test when run
  normally), which connects, starts tasks, and is then SIGKILLed.
- Race tests (`d2`) add CPU burners (one per core) for the test's duration
  to widen scheduler windows; this made the original race fail every time.
- Cleanup: each `Home` kills every recorded task process group and the
  daemon on drop, even when the test panics.

```bash
cd manager
cargo test --features test-clock                         # everything, manual clock (~44s wall)
cargo test                                               # everything, real time (~59s wall)
cargo test --test lifecycle_adversarial                  # adversarial suite
scripts/ablate.sh                                        # ablation check (~8 min, manual clock)
cargo mutants -j 3 --timeout 150 -f src/lifecycle.rs -f src/task.rs -f src/registry.rs \
  -f src/daemon.rs -f src/sys.rs -f src/proto.rs          # mutation score (manual clock via .cargo/mutants.toml)
```

Measured on an M-series Mac with a warm build (129 passing tests with the
feature, 127 without; the difference is the two `clock` unit tests):

| | `cargo test` wall | per-test time, summed serially | `cargo mutants -f src/lifecycle.rs` (42 mutants, -j 3) |
|---|---|---|---|
| before (real time only) | 48s (50s re-measured) | 182s | 324s |
| `--features test-clock` | 44s | 77s | 226s (−30%) |
| plain, after | 59s (+2 canaries, 8s) | — | — |

Per binary with the feature: unit 0.2s, `lifecycle_adversarial` 18s,
`mutation_gaps` 2.6s (was 14s), `observability` 2.3s (was 5s), `protocol`
7.6s, `timing_canary` 7.7s. Binaries run one after another; tests inside a
binary run in parallel. The wall time is now bounded by tests that are not
about the timers: `c6` (256 MiB through a stuck watcher, ~15s), the real-time
`protocol::t09`, and the canaries. The timer-bound tests dropped from 5–12s
each to about 1s (`d5` 1.1s, `d6` 1.0s, `d4` 0.8s). `d3` no longer sleeps a
fixed 1.5s per round (15.8s → 1.5s). Both modes are stable with 3–4
concurrent copies of the integration suites.

## Time in tests

The daemon's own timers are the 5s idle grace, the 2s kill grace (stop
reaper and graceful shutdown) and the 500ms leftover-group poll (now only a
fallback, for a runner that died without reporting). Those are the only
timers the manual clock covers; they go through `src/clock.rs`. The task
runner's own timers (the 2s lifeline grace and the guardian poll — 100ms
for the first second, then 1s backoff) run in the runner process on real
time via `std::thread::sleep` and are never advanced by the manual clock.
In a normal build clock.rs is `tokio::time::sleep`. With the `test-clock`
cargo feature **and** `PI_FAMULUS_TEST_CLOCK=manual` in the daemon's environment,
the daemon's timers run on a manual clock instead:

- Virtual time starts at 0 and moves only on `clock_advance {ms}`.
  `clock_status` lists the pending timers by label (`idle`, `kill-grace`,
  `shutdown-grace`, `group-poll`) and time left.
- Both requests exist only under the feature. They are sent as the first
  frame of a fresh connection, without hello, so they never count as an
  active connection and never cancel the idle timer.
- Everything else stays on real time: child processes, `timeout_ms`, the
  hello timeout, record timestamps.
- The daemon keeps accepting during graceful shutdown (see D8/D8b). That is
  also what lets a test step the 2s grace that holds a shutdown open.
- A manual-clock daemon never idles out by itself, so one left by a killed
  test binary (Ctrl-C, a timeout; `Drop` does not run) would hold its tasks
  forever. `tests/common` sets `PI_FAMULUS_TEST_OWNER` to the test's pid on every
  daemon it starts. Under the feature the daemon polls that pid on real time
  and exits (a crash, so the lifelines take every task down) once it is
  gone (D4d).

`tests/common` drives it:

- `Home::advance(label, ms)` waits until a timer with that label is armed,
  then advances. A step can never race ahead of the daemon scheduling the
  timer.
- `advance_almost(label, total)` stops 1 ms short and asserts that the timer
  is still pending with exactly 1 ms left. A shorter or longer constant
  fails there, deterministically.
- `advance_past()` takes the last millisecond.
- `advance_now(ms)` shows that a cancelled countdown does not fire.

Without the feature, every step is a real sleep: `advance_almost` sleeps
60% of the timer, and `advance_past` does nothing. So plain `cargo test`
still runs every test against real time, with margins.

`tests/timing_canary.rs` always uses the real clock (`Home::new_real`) and
pins the actual values:

- the idle grace: a task is SIGTERMed 4.9–6.5s after the last client leaves;
- the kill grace: a TERM-ignoring task is SIGKILLed 1.9–3.5s after `stop`.

`protocol::t09` also still runs on real time. Shortening either constant
turns both canaries red. So does the manual-clock suite: 6 lifecycle tests,
including `d7` through the `advance_almost` pending check.

Why not an existing tool:

| Option | Why not |
|---|---|
| tokio `test-util` (`time::pause` / `advance`) | Controls only the runtime of the process under test, and only a current-thread runtime. These tests are black box: the daemon is a separate binary with a multi-thread runtime, and the test cannot reach its runtime. Paused time also auto-advances whenever the runtime is idle. The daemon is idle exactly while it waits on real children, so the 2s kill grace would fire at once, before a TERM-ignoring child could be observed alive. |
| turmoil | Simulates hosts and TCP/UDP inside one process. There are no unix sockets, no `fork`/`exec`, and no signals or process groups. Those are what the lifecycle is made of. |
| madsim | Deterministic simulation, but it replaces tokio and std at build time for the whole crate, and it cannot run real child processes. The kill-grace and group tests need real processes that ignore SIGTERM. |
| Env var that shortens the constants | Faster, but it changes the values under test and still races on the wall clock. A manual clock keeps 5000/2000 exact and makes the ordering deterministic. |

The feature is off by default and adds no dependency. Release builds do not
contain the debug requests (`#[cfg(feature = "test-clock")]` on the protocol
variants). `PI_FAMULUS_TEST_CLOCK` alone does nothing without the feature.

## Lifecycle state-transition table

Columns: **Before** = covered by the tests that existed before this work
(`protocol.rs` + unit tests); **After** = test that now covers the cell.
`FIXED` = the cell was a bug found by this suite; its test failed against
the original code and passes now (see "Bugs found").

### Connection

States: `accepted` (socket open, no hello) → `active(cli|ext)` → `closed`.

| # | State | Event | Next state | Side effects | Before | After |
|---|---|---|---|---|---|---|
| C1 | accepted | no hello (silent peer) | accepted until 10s, then closed | not counted as active: cannot hold the daemon alive | no | `d7` |
| C2 | accepted | invalid hello (missing session_id / pi_pid, path-like id, v≠1, non-hello first frame) | closed | `E_BAD_REQUEST` / `E_VERSION` response; nothing registered | no | `c2`, `f1` |
| C3 | accepted | valid hello (ext / cli) | active | idle countdown cancelled; ext session registered `connected:true` with its `cwd` | `t01` | `t01`, `d6`, `d16` |
| C4 | active(ext S) | another connection says hello for S | closed | old conn gets `session_rebound`, then server closes it; S's tasks and events follow the new conn | no | `c4` |
| C5 | accepted | hello while shutting down | closed | error response; shutdown not cancelled | no | `d8` |
| C6 | active | peer never reads (slow watcher) | active | its queue caps at 1024 frames; events dropped past that; other clients, the task, and RSS unaffected | no | `c6` |
| C7 | active | frame > 4 MiB (header) | closed | daemon and other conns unaffected; exactly 4 MiB accepted | no | `f1` |
| C8 | active | malformed JSON / second hello | active | `E_BAD_REQUEST`, connection keeps working | no | `f1`, `c2` |
| C9 | active | EOF from a crashed peer (SIGKILL) | closed | session → disconnected, watchers dropped; last conn arms the 5s idle countdown | clean close only (`t09`) | `d5`, `s3` |
| C10 | active | a response would exceed 4 MiB (escaping, huge echoed id) | active | output chunks are cut to fit; any other oversized response becomes `E_INTERNAL`; an unanswerable one is dropped; the connection keeps serving; exactly 4 MiB is delivered | no | **FIXED** `o3`, `o3b`, `o3c`, `g9` |

### Session (extension)

| # | State | Event | Next state | Side effects | Before | After |
|---|---|---|---|---|---|---|
| S1 | — | ext hello | connected | appears in `status.sessions` with pi_pid and cwd | `t01` | `g5`, `d16` |
| S2 | connected | its connection drops, other clients remain | disconnected | tasks keep running past the 5s grace | no | `s3` |
| S3 | disconnected | hello with the same session_id (`pi --resume`) | connected | sees its tasks; receives their `task_exited` | no | `s3` |
| S4 | connected | duplicate hello | connected (new conn) | see C4 | no | `c4` |
| S5 | connected | `shutdown_session` | connected | exactly its running tasks stopped (`killed`) and reported; leftover groups of its finished tasks killed but not reported; other sessions untouched; cli → `E_SESSION_REQUIRED` | no | `s5`, `t6d` |
| S6 | any | cross-session stop/list/watch | unchanged | `E_FORBIDDEN`, or list silently scoped | list only (`t06`, unit) | `s3`, `g4` |

### Task (§3.4 state machine)

| # | State | Event | Next state | Side effects | Before | After |
|---|---|---|---|---|---|---|
| T1 | running | exit 0 | completed | persisted with real timestamps; `task_exited` to owning session; `wait` wakes | `t02`, `t04` | `t2` |
| T2 | running | exit ≠ 0 / killed by an outside signal | failed | exit_code, or signal name (`"SIGKILL"`) recorded | unit only | `t2` |
| T3 | running | `timeout_ms` elapses | killed | group SIGKILLed; a task finishing earlier is unaffected | no | `t3` |
| T4 | running | `stop` | killed | SIGTERM to the group first (TERM handlers run) | `t08` (status only) | `t4` |
| T5 | running | `stop`, task ignores SIGTERM | killed after ≥2s | SIGKILL after the grace, not before; `signal:"SIGKILL"` | no | `t5` |
| T6 | running | `stop` with grandchildren | killed | whole process group dies | unit (`signal_group`, not via daemon) | `t6` |
| T6b | running | `stop`, leader dies on SIGTERM, grandchild ignores it | killed | the group is SIGKILLed after the grace even though the leader is gone | no | **FIXED** `t6b` |
| T6c | completed, group lingering (runner guards it) | manager shutdown | completed | the leftover group dies with the manager (§3.2) | no | **FIXED** `t6c`, `g11` |
| T6d | completed, group lingering | `stop` / `shutdown_session` | completed | leftover group TERM → 2s → KILL; status unchanged | no | `t6d` |
| T6e | completed, group lingering | last member exits | completed | the guardian runner exits, so the group is no longer tracked: never signalled again, not counted at shutdown; the guardian is idle meanwhile | no | `g11`, `g14` |
| T7 | running / group lingering | manager dies without shutting down (`kill -9`, panic) | — | every runner sees its lifeline break: group SIGTERM → 2s → SIGKILL, grandchildren, SIGTERM-ignoring children and a finished task's leftover included | `t12` asserted the task **survived** | **CHANGED** `d4`, `d4c`, `t12`, `p2` |
| T8 | running (on disk) | manager restart after a crash | orphaned | `end_reason:manager-crash`, `ended_at` set, persisted, counted in manager.log; output size from the file; **no pid is signalled**; nothing re-adopted | `d4` re-adopted live pids | **CHANGED** `d4`, `d4b`, `g6` |
| T9 | running | command exits | completed / failed / killed | the runner reports the command's real exit code or signal; the runner's own wait status only when it died without reporting (SIGKILL with its group) | — | `r1` |
| T10 | running | `stop` arrives before `sh` exists (right after start) | killed | a SIGTERM that reached only the runner is forwarded once `sh` is in the group | — | `p1`, `t5b` (manual clock) |
| T11 | running | manager shutdown (idle / `shutdown` / SIGTERM / SIGINT) | killed | persisted as killed; "manager_shutdown" in manager.log | pid-dead only (`t09`) | `d5`, `d9`, `d15`, `g3` |
| T12 | terminal, no group left | `stop` | unchanged | idempotent ok; unknown id → `E_NOT_FOUND` | no | `t12` |
| T13 | terminal (on disk) | manager restart | unchanged | output served from disk, exact bytes; legacy numeric `signal` still loads | no | `t13`, unit |
| T14 | running | `wait` budget expires | running | `done:false` | `t03` | `t03` |
| T15 | any | `task_exited.signal` / `TaskRecord.signal` | — | signal **name** (`"SIGTERM"`/`"SIGKILL"`), null on normal exit | no | **FIXED** `t5b`, unit |
| T16 | any | `output` read / `watch` event over multi-byte text | — | never split a UTF-8 sequence: no U+FFFD for valid text, no skipped bytes; a partial char is held back while the task runs and delivered at EOF | no | **FIXED** `o4`, `o4b`, `o4c`, `g12` |

### Daemon

States: `absent` → `starting` (claim) → `serving` (≥1 active conn) ⇄ `idle`
(0 active conns, 5s countdown) → `shutting_down` → `exited`; `crashed`
(SIGKILL, stale socket/pid left behind).

| # | State | Event | Next state | Side effects | Before | After |
|---|---|---|---|---|---|---|
| D1 | absent | N clients race to auto-spawn | serving | every client succeeds and is served by the same daemon (its session is in that daemon's `status`); exactly one process holds `manager.lock` (held while it runs, free once it exits); the process count converges to 1 within 2s (a redundant daemon exits "already running" without binding) | no | `d1` |
| D2 | crashed | N clients race over stale files | serving | clients never delete files; exactly one reachable daemon; no client fails | no | **FIXED** `d2` |
| D3 | absent | N `daemon` processes at once (no spawn lock) | 1 survivor | lifetime lock on manager.lock: losers exit 0 "already running" | no | **FIXED** `d3` |
| D4 | crashed | restart | serving | lock holder removes stale files; T7/T8 applied; no client is connected yet, so the 5s idle countdown is armed from boot (§3.2) | `t12` | `d4` |
| D5 | serving | last client process SIGKILLed | idle → shutting_down after 5s → exited | nothing touched during the grace; then SIGTERM → 2s → SIGKILL (grandchildren too), records `killed`, socket + pid removed, exit 0 | `t09` (in-process close) | `d5` |
| D6 | idle | hello within 5s | serving | countdown cancelled; restarts from zero when that client leaves | no | `d6`, `d6b` |
| D7 | idle | only a silent (no hello) connection | exited | — | no | `d7` |
| D8 | shutting_down | hello (connection accepted before or during shutdown) | shutting_down | refused at once (`manager is shutting down`); shutdown completes | no | **FIXED** `d8` |
| D8b | shutting_down | CLI command | serving (successor) | the CLI waits for the old manager to exit, then spawns a successor; no stall, no error | no | **FIXED** `d8b` |
| D9 | serving | cli `shutdown` | exited | kills tasks even with an extension still connected; files removed; ext sees EOF | no | `d9` |
| D10 | starting | stale pid (dead) + socket | serving | client path recovers with a new pid | unit, `t12` | `d10` |
| D11 | starting | socket without pid file / corrupt pid file | serving | cleaned by the lock holder | unit | `d11`, `d11b` |
| D11b | idle | clients leave one at a time, the last long after the first | exited | the countdown still runs when the last client leaves | no | `g7` |
| D12 | starting | another daemon holds manager.lock | exits 0 "already running" | nothing touched | `t10` | `t10`, `d3`, unit |
| D13 | starting | pid file names a live unrelated process (pid reuse) | serving | identity is the lock, not the pid | no | **FIXED** `d12` |
| D14 | serving | SIGTERM / SIGINT to the daemon | exited | same graceful path as D5 | no | `d15` |
| D15 | serving | `shutdown` / `status` from an extension | serving | shutdown → `E_FORBIDDEN`; status allowed (read-only), returns the hello `cwd` | no | `d16` |
| D16 | serving | Ctrl-C (SIGINT/SIGHUP) to the process group of the client that spawned it | serving | daemon was detached with setsid | no | `g2` |
| D17 | any | `doctor` | unchanged | no daemon → removes stale files while holding the lock; live daemon → touches nothing, hello ok | no | `g13` |
| D18 | serving | all watched tasks finished | serving | idle: no busy loop | no | `g10` |
| D25 | starting | the startup scan outlasts the spawning client's 2s socket wait (a large home) | serving | the socket is bound before the scan; the client queues in the backlog and is served, not refused | no | `d25` |

**Coverage:** 57 cells (C 10, S 6, T 20, D 21).

| | Covered | Partial | Uncovered | Violated by the code |
|---|---|---|---|---|
| Before (original code, original tests) | 7 | 10 (unit-level, status-only, or in-process close) | 38 | 8 (C10, T6b, T6c, T15, T16, D2, D3, D13) |
| After (fixed code) | 57 | 0 | 0 | 0 |

## Bugs found (all fixed)

Every test below failed against the original code (recorded on the rebased
tree before any src change) and passes now. `t6d`, `o3b`, `o3c`, `o4b` were
added while fixing and were also run against the pre-fix code: all red.

| Bug | Root cause | Fix | Red → green |
|---|---|---|---|
| `t6b` | The stop reaper and the shutdown SIGKILL pass only targeted tasks whose *leader* was still `running`. A leader dying on SIGTERM left SIGTERM-ignoring descendants unkillable. | After the grace, SIGKILL the **process group** whenever it may still have members (`TaskEntry::owns_live_group`). | `t6b` |
| `t6c` | Shutdown only signalled groups of running tasks; a finished task's backgrounded children were never signalled. | At leader exit, `finalize_exit` checks `kill(-pgid,0)`; if members remain the task is marked `group_lingering` and a watcher polls the group every 500ms until it empties (POSIX never reuses a pid while its group exists). Shutdown, `stop` and `shutdown_session` all include lingering groups. | `t6c`, `t6d`, `g11` |
| `d2` | Client §3.1 step 5 read a stale pid file, then deleted socket + pid — possibly those of a daemon another client had just spawned. | Clients never delete socket/pid files. Only the daemon holding `manager.lock` cleans up. | `d2` |
| `d3` | `claim_pid` was check-then-act: a daemon's socket, bound before its pid file was written, looked like a zombie to a concurrent daemon, which unlinked it. | The daemon takes an exclusive flock on `manager.lock` (O_CLOEXEC, so tasks never inherit it) and holds it for its lifetime. The holder is the only daemon, so any socket/pid it finds is stale. | `d3`, unit `claim_is_exclusive_and_ignores_pid_liveness` |
| `d12` | Identity was `kill(pid,0)` on manager.pid; a reused pid blocked startup forever. | Same lock: identity is the lock, the pid file is informational. `doctor` also uses the lock and cleans only while holding it. | `d12`, `g13` |
| `o3` | Reads were capped at 1 MiB of raw bytes, but control bytes JSON-escape to 6 bytes: the >4 MiB response made `write_frame` fail and the writer task exit, and the connection went mute. | `utf8_chunk_len` cuts each chunk so its escaped size fits `CHUNK_JSON_BUDGET` (4 MiB − 64 KiB). `respond` turns any other oversized response into `E_INTERNAL`; the writer drops an unsendable frame instead of exiting. | `o3`, `o3b`, `o3c`, `g9` |
| `o4` | Chunks were lossy-decoded per read; a `max_bytes` or pipe boundary inside a multi-byte char produced U+FFFD while `next_cursor` skipped the bytes. | Reads fetch `cap + 3` bytes and cut at the last char boundary (a first char wider than `max_bytes` is sent whole, so reads always progress). A truncated tail is held back while the task runs. Watch events carry an incomplete tail over to the next pipe read and flush it at EOF. | `o4`, `o4b`, `o4c`, `g12`, unit `chunk_len_*` |
| `d8b` | Once shutdown began, the accept loop was gone but the socket stayed bound: a client connecting in that window waited for its 30s response timeout, then failed (was deferred in part A). | The daemon keeps accepting during shutdown and refuses hello at once; a client refused with `manager is shutting down` waits (≤ 5s) for that manager's pid to exit, then spawns a successor. | `d8` (fresh connection), `d8b` |
| `t13` flake | `finalize_exit` persisted `output_size` at child exit, before the tee had drained the pipe. After a restart the loaded record reported too few bytes (0 under load; seen once in a stress run as `t13` `total_size` 0 vs 13893). | `scan_tasks` takes `max(record, file length)` for every loaded record, not only re-adopted ones: the file is append-only and can no longer grow. | unit `scan_recovers_output_size_of_terminal_records_from_the_file` |
| `d11` flake (test harness) | The test made its "dead" socket with `UnixListener::bind` + `drop` inside the multi-threaded test binary. On macOS, std sets FD_CLOEXEC only after `socket()` returns, so a child spawned by another test thread in that window inherits the socket and keeps it listening after the drop. `ls` then connected, got no hello answer, and hit the 10s test timeout. A standalone repro under heavy concurrent spawning: 833 of 3000 dropped listeners still accepted connections. Keeping the listener open in d11 reproduces the exact failure (`cli ["ls"] did not finish within 10s`). Not product behaviour: the daemon binds its listener before it spawns any task, and a task inherits only fds 0–2 (checked by listing `/dev/fd` from a task). | `common::dead_socket` checks that a connect is refused and otherwise retries on a fresh inode (0 of 3000 left live in the same repro). Used by `d11` and `g13`. | `d11`; full suite 5× × 3 copies: plain 15/15, test-clock 15/15 |
| `e1` flake (events.jsonl order) | `finalize_exit` published a task's terminal status under the state lock but wrote its `task.exit` line after unlocking. A client that saw the task finish (`list` served by another worker) and then disconnected could get `session.disconnect` into the file first (seen once in the 10×3 stress run: disconnect at …372 ahead of exit at …371). A reader of events.jsonl right after a finished `wait`/`list` could also miss the line. `task.start` was written after the exit watcher was spawned, and `task.background` after unlocking, so both could land after a fast `task.exit`. | Every task event line is written under the state lock, before the state it records becomes visible: `task.start` before the task is inserted, `task.exit` before `status_tx` fires (also in the shutdown force-finalize pass), `task.background` together with `backgrounded_at`. | `e5` (100 fast tasks, `list` from a second connection, then read the file). Red-first needed a 50 ms sleep in the old unlocked window (`round 0: … task.exit is not written yet`); after the fix it stayed green with the same sleep in place. |
| `e4` hang (`events -f` gap) | `events -f` printed history, *then* took every file's length as its follow offset. A line appended in between was skipped for good. `e4` appends to a new session the moment history is out; in one stress run the follower missed it, and the test's unbounded `read_line` blocked for hours until the follower was killed. | History reading returns, per file, the offset just past the last complete line it consumed, and following continues from exactly there (no gap, no duplicates). `e4` reads through a channel with a 5s bound, so a miss fails instead of hanging. | `e4`. Red-first: with a 300 ms sleep in the old window, `events -f never printed the line appended right after its history`; green with the same sleep after the fix. |
| `d6b` flake (manual-clock helper) | `Home::advance` waited for "any timer with that label". Cancelling the idle countdown is an async `abort()`, so under load the aborted timer was still listed, and the helper stepped 4,999 ms before the daemon had armed the new countdown (`pending: [idle due 0]` at t=6999). | `advance(label, ms)` waits for a `label` timer with at least `ms` left; `advance_almost(label, total)` for one with exactly `total` left (freshly armed). A leftover never has that much left, because the clock has moved since it was armed. A shortened constant still fails deterministically. | test-clock 5×3 stress: 14/15 before, 15/15 after |
| `t14` (descriptor leak into tasks) | Tasks inherited every daemon fd that lacked close-on-exec. On macOS, std sets FD_CLOEXEC only after `accept()` returns, so a task forked in that window got a copy of a client connection. That client then saw no EOF when the daemon closed it (session rebind, shutdown) until the task exited. The same happened to anything the daemon itself inherited without the flag. Found while root-causing `d11`. | The `pre_exec` hook that runs `setsid` now marks every fd ≥ 3 close-on-exec: `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` on Linux ≥ 5.11, otherwise an `fcntl` loop up to a bound computed before fork: the highest fd open in `/dev/fd` plus 64 slack, never above the soft RLIMIT_NOFILE. The limit alone is often 10^6, and even capped at 65536 it cost every spawn ~65k syscalls. That load made two existing `task.rs` unit-test races show up (next row). Nothing in the hook allocates. The fds are marked rather than closed, because std reports exec failures over a close-on-exec pipe that must stay open until exec. Applies to tasks and to the daemon the CLI spawns. | `t14` (the daemon holds an inherited fd 20 and two client connections; the task lists its own fds): red before (`"fd 20\nend\n"`), green after. `s6` (40 session rebinds while 120 tasks spawn; each old connection must see EOF within 2s) guards the behaviour; its pre-fix red is only statistical (a microsecond window), so it passed before the fix too. |
| `task.rs` unit-test races (macOS) | Found while verifying the fd fix in a clean copy, 1 run in 300. `spawn_captures_merged_output_and_exit` called `getpgid` on a task that had usually already exited; on macOS a finished process is already gone for `getpgid`. `signal_group_kills_whole_tree` required `Ok` from signalling a dead group, but while the reparented grandchild is still an unreaped zombie, macOS answers EPERM, not ESRCH. Callers ignore that result either way. | The task reports its own process group (`ps -o pgid= -p $$`); the dead-group call accepts EPERM. The test-only `sys::getpgid` is removed. | full unit harness 300× in a loop: 0 failures |
| `d1` flake (process-count snapshot) | `d1` counted daemon processes once, right after the 12 clients returned, and required exactly one. It failed twice (under mutation load in part A, and once in a clean-copy test-clock run), and the diagnostics were lost both times. A client that loses the race can spawn a redundant daemon, which exits "already running" without binding; a snapshot can catch it before it exits. The count was also standing in for the invariant: the old test never checked that the clients were served by that daemon. | Nothing in the product: the lifetime lock already guarantees one serving daemon. `d1` now asserts the invariant itself. (a) Each client opens its own session with `start --session`, and all 12 must appear in the survivor's `status` (sessions live only in the memory of the daemon that served them). (b) `manager.lock` is held while the socket's daemon runs, and free the moment it exits. (c) The process count converges to 1 within 2s. Failures print `ps -ww` lines and manager.log. | `d1` 100 rounds × 3 parallel copies in each mode: plain 300/300, test-clock 300/300. Proof it can fail: the `daemon-lifetime-lock` ablation turns it red at (b) (`nobody holds manager.lock`); expecting one session no client opened turns (a) red. |
| `u1` `u5` `u11` on the macOS CI runner (test timing) | The upgrade tests streamed output with `sleep 0.01`/`0.02` per line and expected the loop to finish in ~3 s. On the GitHub macOS runner a sub-100 ms `sleep` takes ~95–115 ms (100 × `sleep 0.02`: 9.5–11.7 s; 100 × `/usr/bin/true`: 0.15 s; the same under pi-famulus: no slower), so a 300-line stream needed ~30 s and the tests' 10 s request timeout fired. It first looked like tasks stalling after an upgrade; a pump trace showed reads continuing at the same rate before and after the upgrade (u1 126 vs 148 B/s). Not product behaviour. | Loops sleep 0.1 s every N lines (same total ~3–5 s, output still straddles the upgrade); `Conn::request` waits `budget_ms` + 5 s for a request that carries one. | A `sleep` shim in PATH that rounds anything under 0.1 s up to 0.1 s: the old tests fail exactly u1, u5, u11 (u9 passes); the new ones pass. |
| `t13` hang (stale leftover-group flag, manual clock) | Seen once in a test-clock stress run: the daemon never exited after `shutdown`. `finalize_exit` marks a finished task `group_lingering` when `kill(-pgid, 0)` still succeeds. On macOS, just after the leader is reaped, that probe can answer EPERM for about a millisecond even though the group is already empty (diagnostic: EPERM at exit, `ps` shows no member, gone 1 ms later). Only the 500 ms `group-poll` clears the flag. Under the manual clock that poll never runs unless a test steps it, so shutdown saw a "leftover" group, sent SIGTERM to it, and waited forever on `shutdown-grace` (`clock_status`: `group-poll` 500, `shutdown-grace` 2000 at t=0). Plain mode lost only 2s. Any group that empties between two polls leaves the same stale flag. | Shutdown re-probes leftover groups before it signals them (`TaskEntry::refresh_lingering`), and a group that still looks alive gets a second probe 20 ms later. An emptied group gets no SIGTERM and no grace. The probe runs again before the SIGKILL pass, so a pgid that emptied during the grace is not signalled. A live leftover still gets TERM, grace and KILL (`t6c`, `g11`). | `t13b` (the leader leaves `sleep 0.3` behind; once it exits, shutdown with no clock step): red before (test-clock: `shutdown waited on an emptied group`; plain: `killed 1 leftover process group(s)`), green after. `t13` under load (6 copies + 3 full-suite loaders): 2 hangs in 360 runs before, 0 in 180 after; 200 × 3 copies with 3 loaders: 600/600. |
| `t5b` | `signal` was an integer on the wire and on disk; §3.3 and the extension type say `"SIGTERM"`/`"SIGKILL"`. | `signal` is a name (`proto::signal_name`) in `task_exited`, `TaskRecord` and the CLI `EXIT` column (widened to 7). Legacy numeric records still load (converted). The extension only tests truthiness / displays it; verified with `tsc`, its unit tests, and its real-binary integration tests. | `t5b`, `t2`, unit `signal_names_on_wire_and_legacy_numbers_load` |

## Observability contract (manager + CLI side)

`tests/observability.rs` covers the contract black-box. Extension-owned
files (agent records, transcripts, extension events) are written as
fixtures in the contract's format, because the extension side may land later.

| Requirement | Test |
|---|---|
| `start.origin` stored; `mark_background` → `backgrounded_at` (first time kept, persisted) | `p1` |
| `stop.reason` → `end_reason` (`stopped:tui/cli/tool`, `timeout`, `rate-limit`, `session-end`; none = `stopped:tool`; unknown → `E_BAD_REQUEST`); natural exit → `exited`; `timeout_ms` → `timeout`; `shutdown_session` → `session-end`; first reason wins; `task_exited.end_reason` | `p1` |
| `manager-shutdown`, `manager-crash` | `p2` |
| hello `extension_version`/`protocol` stored per session; `status.protocol`; `connected_at` kept across reconnects; `last_seen` | `p3` |
| manager writes `session.connect/disconnect`, `task.start` (command ≤ 200 chars, origin, pid), `task.background`, `task.stop`, `task.exit`, `daemon.start/shutdown` | `e1` |
| CPU accounting: `cpu_user_ms`/`cpu_sys_ms`/`max_rss_kb` on the live and persisted record and on `task.exit`, covering CPU spent only by a grandchild; kept through a cooperative stop; absent (not zero) after the hard timeout; `ls` CPU/CORES columns (`-` while running), `ls --json`, `show`'s cpu line and its "not measured" | `p4`, `c1` (red when the record takes the wrong runner value), unit `runner::*`, `task::runner_*` (red with `RUSAGE_SELF`) |
| a task event line is on disk before anyone can see the state it records, in causal order (`task.start` < `task.exit`) | `e5` |
| every line < 4 KiB, oversized fields truncated (`truncated:true`), ids never cut | `e2`, unit `events::*` |
| concurrent appends (8 extension-style writers × 300 lines of 1–3.5 KiB, plus the manager) never interleave or lose lines | `e3` |
| `events`: malformed lines skipped with a stderr count; `--id` matches `id`/`ids[]`/`child_id`; `--session`, `--since`, `--json`, `-f` (incl. new sessions); cross-session time order; never starts the daemon | `e4` |
| pager: on a terminal (script(1) pty) listings go through `PI_FAMULUS_PAGER`, else `PAGER`; not with `--no-pager`, `cat`, or piped stdout; a bare `less` runs as `less -FRX` | `c10`, unit `pager::*` |
| `ls`: columns, running-only default, `-a` (connected sessions' finished work), running first then newest first, an agent's TIME = its last transcript message, agents included, SESSION shortest unique prefix ≥ 8, CJK display-width truncation, `--json`, `--session`/`--cwd`/`--since`, bad duration rejected | `c1`, `c1b`, unit `fmt::*`, `inspect::*` |
| `show` for sh_/ch_/run_ (header, origin, backgrounded, wake emitted→delivered, last 10 lines; agent error/tool calls/shells/prompt/result tail 20), fuzzy + `--json`, one-line not-found with closest match | `c2` |
| `stats`: grouping by agent (child record name, bare id, `main <session>`), kind, and both; CPU sums measured tasks only and CORES divides by their wall alone; UNMEASURED/KILLED/KILLED-WALL; gone sessions count; `--session`/`--since`/`--json`; text header and TOTAL; bad `--by` rejected; never starts the daemon; live daemon numbers | `c11`, `p4`, unit `stats::*` (red when CORES divides by all wall time) |
| `work_kind` on task rows of `ls --json` and in `show` (agents: none); the classifier on compound commands from a real run (heaviest simple command wins, `bash -c` recursion, heredoc bodies skipped, `$(…)` not surfaced, suite vs targeted) | `c1`, `c2`, unit `workkind::*` (red when the last simple command wins instead of the heaviest) |
| `agent` (preamble hidden, `--full`), `log`/`tail -f` on ch_ ids, `output`/`wait` on ch_, `stop` on an agent refused with the contract message | `c3` |
| `stop` → `stopped:cli`; "already finished (<reason>)" | `c4` |
| `sessions`: connected only, gone sessions hidden but `show`/`events` still reach them, counts, `--json`, no spawn | `c5` |
| gone-session retention: swept after `goneSessionRetention`; connected and still-running sessions kept; swept tasks leave `show`/`ls` | `g1` (red with the sweep disabled) |
| `doctor` flags an invalid `goneSessionRetention` | `g2` |
| finished-task retention: in a connected session, a finished task's record, output and stderr are deleted after `finishedTaskRetention`; running tasks, lingering groups, agents and events stay; swept tasks leave `show`/`ls` | `g15` (red with the task sweep disabled) |
| finished-task retention: a task whose files could not be deleted (e.g. an unwritable tasks dir) keeps its record for the next sweep to retry, instead of being forgotten while its files remain on disk | `g15b` |
| `doctor` flags an invalid `finishedTaskRetention` | `g2` |
| `status`: human uptime, counts incl. agents, protocol, `--json`; not running → exit 1, no spawn | `c6` |
| `output --max-bytes` is a total cap (UTF-8 safe); SIGPIPE → exit 0, silent (output, ls, events, log); human timestamps in `log` | `c7` |
| `doctor`: home missing (not created), config.json, managerPath, stale agent records, orphan pids, socket path length, exit status; protocol per session | `c8`, `protocol::t11`, `mutation_gaps::g13` |

Interpretation decisions where the contract is silent (also in design §3.3
and docs/cli.md): `PROTOCOL = 2`; a stop without `reason` ends as
`stopped:tool`; `shutdown_session` maps to `session-end`; a task whose
manager crashed ends as `orphaned` / `manager-crash` at the next startup;
`daemon.*` events go to `<home>/events.jsonl`; doctor counts stale
socket/pid files it removed as `fixed` (exit 0), not failures;
`status`, `sessions`, `show`, `agent`, `events`, `log`/`tail` and
`shutdown` never start the daemon.

## Mutation score

`cargo mutants 27.1.0`. A timeout is an infinite loop the tests detect by
hanging, so it counts as killed; score = (caught + timeout) / viable.

History (five files `lifecycle task registry daemon sys`, old code):

| Run | Viable | Killed | Missed | Score |
|---|---|---|---|---|
| Original tests only | 191 | 129 | 62 | 67.5% |
| + lifecycle_adversarial + g1–g8 | 191 | 170 | 21 | 89.0% |

Fixed code, six files (`proto.rs` added: it holds the frame codec and
signal names). The full run was made first, then the survivors drove
g9–g13, o4c, and unit tests for `utf8_chunk_len`, `signal_name` and
`new_request_id`. Two `--iterate` passes re-tested every survivor
(line-shifted mutants of edited code were re-tested too and caught again):

| File | Viable | Full run: killed / missed | After survivor tests: killed / missed | Score |
|---|---|---|---|---|
| daemon.rs | 116 | 95 / 21 | 112 / 4 | 96.6% |
| lifecycle.rs | 33 | 25 / 8 | 29 / 4 | 87.9% |
| proto.rs | 72 | 37 / 35 | 59 / 13 | 81.9% |
| registry.rs | 19 | 18 / 1 | 19 / 0 | 100% |
| sys.rs | 22 | 18 / 4 | 20 / 2 | 90.9% |
| task.rs | 82 | 77 / 5 | 81 / 1 | 98.8% |
| **total** | **344** | **270 / 74 (78.5%)** | **320 / 24** | **93.0%** |

`client.rs` (the touched functions `connect`, `cmd_doctor`, `cmd_list`,
`ls_header`, `format_ls_row`): 34 mutants, 2 unviable, 32 viable, 25
killed, 7 missed (78.1%). All 12 `cmd_doctor` mutants are killed (`g13`).

The 24 manager survivors and 7 client survivors, classified as (a) missing
test, (b) equivalent (no observable difference under the contract), (c)
dead or unneeded code:

| Mutant | Class | Why |
|---|---|---|
| daemon.rs:422:44, 430:36 idle-timer guards | b | The two guards duplicate each other, and every disconnect re-arms anyway (the sibling guard was a real gap, killed by `g7`). |
| daemon.rs:531:20 access_for guard → true | b | Differs only for a cli hello carrying a session_id, which no client sends. |
| daemon.rs:739:40 ring `avail` `-`→`+` | b | `slice()` clamps to the ring; the ring is a pure cache of `.output` (`g1` checks bytes exactly). |
| lifecycle.rs:93:23, 138:19, 176:19, 203:19 NotFound / WouldBlock guards → true | b | Only other io errors (read-only home, EACCES on a lock) differ, and only in the error message; startup fails either way. |
| proto.rs:387:19 UnexpectedEof guard → true | b | The daemon treats EOF and read errors identically (close the connection); the CLI only changes an error message. |
| proto.rs:446:26 `\|`→`^` on the variant byte | b | `& 0x3f` already cleared bit 7, so `^ 0x80` equals `\| 0x80`. |
| proto.rs:433–437 ×11 in `random_bytes` fallback | **c** | The fallback PRNG runs only when `/dev/urandom` cannot be read, which does not happen on the supported unix targets. **Slop candidate** (not on the approved removal list, so kept). |
| sys.rs:28:34, 42:34 `setsid() == -1` → `== 1` | b | `setsid` cannot fail in a freshly forked non-leader child and never returns 1. |
| task.rs:22:37 RING_CAPACITY `64*1024`→`64+1024` | b | Ring is cache-only; a smaller ring only changes performance. No black-box test can tell the ring exists. |
| client.rs:149:20 `attempt > 0` → `<` in `connect` | b | Only changes the error path after two failed attempts (one extra spawn try before erroring). |
| client.rs:161:20, 167:20 delete `!` in `connect` | b | Only changes which error text is kept; a successful spawn is still found on the retry. |
| client.rs:239:40 ×2, 242:28 ×2 in `cmd_list` | a (not this work) | The "N terminal hidden, use -a" hint arithmetic from the KIND/agent-records change. Untested, but not code touched here. |

### Observability and test-clock code (parts B and C)

`cargo mutants --features test-clock --in-diff` over every `src` change
since the part-A docs commit (`6ec4135..`): 493 mutants, 35 unviable,
**458 viable, 330 caught + 12 timeouts = 74.7%**. The run took 68 min at
`-j 4`. `clock.rs`: all 15 viable mutants killed.

The two manager survivors were followed up:

- `daemon.rs` `touch_session` → `()`: **dead code, removed**. `status`
  reports `now` as `last_seen` for a connected session, and disconnect
  sets `last_seen` itself, so the per-request update could never be seen.
- `daemon.rs` `delete !` in `spawn_adopted_poller`: **missing test**. The
  poller then stops sleeping after its first tick and spins. `g14` (daemon
  CPU with one re-adopted task) now kills it. It first survived `g14`
  because the manual clock never passed the first `adopt-poll` tick; the
  test now advances it.

After those: 343 / 457 = **75.1%**. The other 114 survivors are all in the
CLI presentation code. Classes: (a) missing test, (b) equivalent or not
observable in tests, (c) dead.

| Area | Survivors | Class | What |
|---|---|---|---|
| `fmt.rs` `char_width` ranges | 11 | a | Only a few CJK and emoji ranges are exercised. `\|\|`→`&&` on the others goes unseen. |
| `fmt.rs` `human_duration`, `datetime`, `short_time`, `local` | 10 | a / b | Boundary values (exactly 60s, 60m, 24h) are untested (a). Human timestamps are only checked for shape, not value (a). The tz offset arithmetic is equivalent on a UTC-offset-0 check (b). |
| `events.rs` `encode_line`, `shrink_longest_string` | 18 | a | Truncation arithmetic at the edges of the 4 KiB cap. The tests assert that every line is under the cap and still valid JSON, not the exact size removed. Some `<`/`<=` swaps sit exactly on the cap (b). |
| `sys.rs` `stdout_tty_columns`, `inspect.rs` `term_width` | 12 | b | The tests never run on a TTY, so terminal width is always unknown. |
| `inspect.rs` `cmd_ls`, `render_ls`, `session_views` | 16 | a | The "N hidden, use -a" hint counts, session sort order for equal timestamps, and the COMMAND width arithmetic. |
| `inspect.rs` transcripts, `wake_summary`, `cmd_show`, `render_event`, `cmd_events`, `cmd_sessions`, `cmd_status`, `wait_agent` | 19 | a | Rendering details (preamble detection edges, tail windows, `-f` poll bookkeeping), and the `wait` budget arithmetic for agents. |
| `client.rs` `cmd_doctor`, `dir_size`, `Report::warn` | 16 | a | The doctor's disk-usage line and warnings are printed but not asserted. The protocol-match branch is only tested as a mismatch. |
| `client.rs` `resolve_task_id` closest match | 6 | a | The distance thresholds of the "did you mean" hint. |
| `client.rs` `cmd_kill_session`, `cmd_start` → `Ok(())` | 2 | a | Convenience commands without black-box tests. |
| `client.rs` `wait_for_manager_exit` | 2 | b | `&&`→`\|\|` always waits the full 5s and `<`→`<=` changes nothing. Only latency differs, and `d8b` still passes. |
| `client.rs` `cmd_output` | 1 | a | The `--max-bytes` read-size clamp. |
| `out.rs` `bytes` | 1 | b | The EPIPE check vs. other write errors; both end the command. |

deferred: tests for the (a) rows above | impact: CLI rendering regressions (widths, hints, doctor text) would not be caught; no effect on the lifecycle or wire contract | trigger: the first user-visible CLI rendering bug, or before the CLI output is declared stable for scripts

## Changed code for the fixes

| File | Change |
|---|---|
| `src/lifecycle.rs` | `claim_pid` → `claim_daemon` (lifetime flock on `manager.lock`); `clean_if_no_daemon` for doctor; `daemon_lock_path`; re-adopt recovers `output_size` from the file |
| `src/daemon.rs` | holds the daemon lock for its lifetime; process-group lifetime (`owns_live_group`, `spawn_group_watcher`, group SIGKILL in reaper/shutdown/shutdown_session/stop); UTF-8 + escaped-size chunking in `handle_output` and the watch fanout; oversize response → `E_INTERNAL`; writer skips unsendable frames; signal names; adopted poller reduced to the exit poll |
| `src/task.rs` | `utf8_chunk_len`, `json_escaped_len`, `UTF8_LOOKAHEAD`; `read_file_range` via `take().read_to_end`; EINTR arm removed |
| `src/proto.rs` | `signal: Option<String>` with legacy-number deserializer; `signal_name` |
| `src/registry.rs` | `group_lingering`, `owns_live_group` |
| `src/sys.rs` | `group_alive` |
| `src/client.rs` | no client-side file cleanup; doctor uses the daemon lock; EXIT column shows signal names (width 7) |
| `docs/design.md` | §3.1 singleton via `manager.lock`, clients never clean; §3.2 group kill incl. lingering groups; §3.3 signal names, chunk boundary and frame-size rules; §3.4 re-adopt output size |
| `docs/cli.md` | layout (`manager.lock` vs `manager.spawn.lock`), EXIT column, doctor |

## Ablation

`ablation.toml` lists 45 load-bearing mechanisms (32 lifecycle, 13
observability), each with a literal
find/replace and the tests that must go red. `scripts/ablate.sh` applies
each one to a scratch copy (sharing one build cache), first checks that
every listed test passes on the pristine copy (a test that is already red
proves nothing), then runs them one by one and compares the result with
`expect`. Per-test logs go to `$ABLATE_WORK/logs/`. Needs `cargo`,
`python3` ≥ 3.11 (tomllib), `perl`, `rsync`. The runner builds and
tests with `--features test-clock` unless `ABLATE_FEATURES` is set (empty =
real time). The full run takes 476s with the manual clock, against about
20 minutes on real time.

**Runner bug found and fixed (`fix(ablate)` commit).** `fresh_copy` used
`rsync -a`, which restores a file changed by an earlier ablation with the
pristine file's *older* mtime, so cargo kept the ablated binary. Within one
run every ablation edits a file and forces a rebuild, so verdicts were
sound; but a later run's **baseline** could test the previous run's last
ablated binary. That was the one unexplained `c6` baseline failure seen
during part A (the previous run ended on a bounded-queue ablation), which
had been filed as a possible flake. Restores now rewrite differing files
with a new mtime.

Final pass (manual clock, after the test-clock work): **44/44 entries
behave as declared** (43 red, 1 green), with no baseline flakes. The first
run reported `refuse-hello-while-shutting-down` as a stale manifest (see
below); after that fix it was re-run and is red.

| Ablation (mechanism removed) | Tests red |
|---|---|
| idle-grace-5s (5s → 50ms) | 4/4: d5, d6, d6b, t09 |
| idle-shutdown-fires | 4/4: d5, d7, d4, t09 |
| idle-timer-cancel-on-hello | 1/1: d6b |
| sigkill-grace-2s (2s → 0) | 1/1: t5 |
| stop-sigkill-escalation (group SIGKILL after grace) | 2/2: t5, t6b |
| lingering-group-tracking | 3/3: t6b, t6c, t6d |
| shutdown-sigkill-escalation | 3/3: d5, d9, d8 |
| setsid-process-group (tasks) | 4/4: t6, t5, d5, d4 |
| group-signal (kill(-pgid) → kill(pid)) | 3/3: t6, d5, d4 |
| bounded-conn-queue (1024 → 2^28) | 1/1: c6 |
| bounded-tee-channel (64 → 2^28) | 1/1: c6 |
| daemon-lifetime-lock (guard dropped at startup) | 3/3: t10, d3, d1 |
| claim-removes-stale-files | 3/3: d10, d4, t12 |
| clients-never-clean (re-adds the old client cleanup) | 1/1: d2 |
| spawn-lock | 0/2 (declared green, see below) |
| utf8-chunk-boundary | 2/2: o4, o3 |
| escaped-size-budget | 1/1: o3 |
| watch-utf8-carry | 1/1: o4b |
| oversize-response-fallback | 1/1: o3b |
| writer-skips-oversize | 1/1: o3c |
| session-rebound-close | 1/1: c4 |
| refuse-hello-while-shutting-down | 1/1: d8 |
| cleanup-files-on-shutdown | 3/3: d5, d9, d15 |
| frame-size-limit | 1/1: f1 |
| timeout-hard-kill | 1/1: t3 |
| daemon-detach-setsid (auto-spawned daemon) | 1/1: g2 |
| accept-during-shutdown (accept loop stops at shutdown, as before) | 2/2: d8, d8b |
| client-waits-out-shutdown | 1/1: d8b |
| child-fd-hygiene (fd scan disabled; macOS path) | 1/1: t14 |

Changes from the first manifest:

- **pidfile-liveness-daemon / pidfile-liveness-client** are gone with the
  mechanisms themselves. They are replaced by **daemon-lifetime-lock**,
  **claim-removes-stale-files** and **clients-never-clean**. The last is a
  regression check: re-adding the deleted client-side cleanup turns `d2` red.
- **spawn-lock** is now `green` by design. The daemon lifetime lock alone
  guarantees a single daemon (`d1` and `d2` stay green without the spawn
  lock); the spawn lock only avoids starting redundant processes that then
  exit "already running". It is kept as a cheap optimisation, not for
  correctness.
- **refuse-hello-while-shutting-down** first stayed green: once shutdown
  starts, the accept loop has exited, so only a connection accepted
  *before* shutdown can race it. `d8` now opens its connection first.
- **refuse-hello-while-shutting-down** now finds `SHUTTING_DOWN` (the
  message became a shared constant, so the client can recognise it).
- Under the manual clock, **idle-grace-5s** and **sigkill-grace-2s** go red
  through the clock helpers, not by timing: `advance_almost` finds no
  pending timer 1 ms short of 5000 (`d5`), or the timer never shows up as
  armed (`d6b`; `t5` with a zero grace). `protocol::t09` still catches the
  idle case on real time. The canaries pin the real values.
- **bounded-tee-channel** was first declared `green` ("fanout never
  blocks"), which was wrong: the fanout lags the tee at high output rates,
  and without the bound RSS grew ~190 MiB for 256 MiB of output.

## Removed code (slop)

| Removed | Why it was safe | Check |
|---|---|---|
| Output tailing + watch fanout in `spawn_adopted_poller` | Unreachable. The task's stdout pipe dies with the old manager, so the file cannot grow after restart. The one useful effect (recovering `output_size`) moved into `scan_tasks` as a `metadata().len()` read. | `g6` (pre-crash output size and bytes after restart), `d4`, `d4b` stay green |
| `Interrupted` retry arm in `pump_async` | tokio's `AsyncRead` retries EINTR internally and never surfaces it | full suite green |
| Hand-rolled read loop + `Interrupted` arm in `read_file_range` | Replaced by `take(max).read_to_end`, which retries EINTR itself | `read_file_range_offsets`, `g1`, `g8`, `o1`, `t13` green |
| Client-side zombie cleanup in `connect` (§3.1 step 5) | Redundant with the daemon's cleanup, and the cause of bug d2 | `d2`, `d10`, `d11`, `d11b` green; ablation `clients-never-clean` |
| `claim_pid` (pid-liveness identity) | Replaced by the lifetime lock | `t10`, `d3`, `d12` |
| `touch_session` (per-request `last_seen` update, part B) | Unobservable: `status` reports `now` for connected sessions and disconnect sets `last_seen` | `p3`, `c5` green; found as a mutation survivor |

No removal turned a test red.

## Deferrals

- deferred: HELLO_TIMEOUT (10s) close of a silent connection is not asserted, only that it does not keep the daemon alive | impact: a silent peer holds one fd for 10s; not customer-visible | trigger: if connection limits are added
- deferred: Windows. The manager is unix-only (process groups, `setsid`, signals, unix pipes; no Windows `cfg` anywhere), and the extension is not Windows-ready either: tried on `windows-latest` (ci-platforms), 18 of 378 extension unit tests failed. 15 because `famulusPaths()` gives `<home>/manager.sock`, a unix socket path Node cannot listen on or connect to on Windows (`listen EACCES`; it needs `\\.\pipe\...`, design §3.1); 2 because tests assume `/` separators; 1 because the 1 GiB sparse-file tail test times out on NTFS | impact: no Windows support at all, neither manager nor extension | trigger: first Windows build: a named-pipe path in `famulusPaths()`, a Windows manager, then add `windows-latest` to the CI matrix
- deferred: the extension's own connect path (TypeScript) is not changed to wait out a shutting-down manager the way the Rust client now does; the exact protocol to implement is design §3.1 step 6 | impact: an extension connecting in the ≤ 2s shutdown window gets `manager is shutting down` at once instead of a successor | trigger: extension side of this branch's merge (handed to the extension engineer)
- resolved (ci-github-actions): `task_exited.output_size` and the terminal record now cover every byte. The exit watch waits for the pumps to drain before finalizing (restored after the rebase lost it, `t15`), and the output fanout no longer moves a finished record's `output_size` back to its own lagging cursor.
- deferred: the extension's `list` (own session only) is not paged | impact: a single pi session with more than ~4 MiB of task records (thousands of tasks, or very long commands) gets `E_INTERNAL` from `task_list` and its reconnect reconcile | trigger: a session that long-lived, or `task_list` failing with the frame-limit error
- deferred: one record larger than a frame (a command near 4 MiB) still fails a paged `list`, since a page always carries at least one record | impact: `ls`/`sessions`/`show` fail while that record is retained | trigger: a start request with a multi-MiB command
- resolved (manager-lifeline): re-adoption by pid liveness is gone. A crashed daemon's tasks die with it (lifeline), and the next startup only marks their records; it never signals a recorded pid (`d4b`).

## Lifeline and runner (manager-lifeline)

Principle: pi-famulus is the parent of every task. When it ends by any
means, every task's process group goes with it (a descendant that escapes
to a new session with `setsid` is outside the guarantee). There is no crash
recovery.

Every task runs under `pi-famulus __run` (`src/runner.rs`), the leader of
its process group. The runner:
- spawns `sh -c` as a child in the same group (it does not exec; it
  stays alive as group leader);
- holds the lifeline read end at fd 3; the daemon holds the only write end
  (`task::lifeline()`, close-on-exec);
- reports the command's status on a status pipe at fd 4;
- guards leftover children until its group is empty.

The daemon treats "status received" as the task's exit and "runner exited"
as "group empty". A crash leaves only records to mark (`orphaned`,
`manager-crash`).

Bugs found while building it (each caught by a test before the fix):
- `proc_listpgrppids` returns a **pid count**, not bytes; dividing again
  made every group look empty, so leftovers went unguarded (`t6b`, `t6c`,
  `t6d` red; unit `sys::group_members_lists_a_real_group`).
- A SIGTERM **handler** in the runner lost stops that landed between `sh`'s
  fork and exec. The forked child ran the inherited no-op handler; only the
  manual clock showed it (`t5b` red), because on real time the 2s SIGKILL
  masked it. The runner now blocks SIGTERM and the child unblocks it right
  before exec, so the signal waits as pending instead of being lost.
- std's `Command` does **not** reset the signal mask for the child here: a
  blocked SIGTERM is inherited and every stop was ignored (9 tests red).
  Hence the explicit unblock in `pre_exec`.
- A stop right after start could reach the runner **before `sh` existed**
  and stay pending there (`p1` red on the manual clock). The runner
  forwards a pending SIGTERM to its group once `sh` is in it.

Found by the stress run (5 iterations x 3 concurrent copies, load average
~300 on the machine at the time):
- `t13b` (2 of 15 plain runs): a leftover that had just exited still had
  its guardian runner alive (≤ 100 ms), and `refresh_lingering` probed
  `kill(-pgid, 0)`, which counts the runner itself, so shutdown signalled
  and waited on a "leftover" that was only the runner. Shutdown and the
  fallback group poll now ask `group_has_others`: members other than the
  leader (the runner, alive or an unreaped zombie).

- `task::tests::repeated_start_stop_stress` (4 of 15 plain runs): a
  group SIGKILL sent right after spawn can arrive while the runner is
  forking `sh`. On macOS the half-created child can miss a signal that is
  delivered to the group during the fork, so `sh` (and its `sleep 30`)
  survived, holding the output pipe open. The same race existed before
  with `sh` forking its own children, but a second fork at task start
  made it common. Every daemon SIGKILL site (timeout, stop reaper after
  the grace, shutdown survivors) now uses `kill_group_hard`: SIGKILL, then
  again every 5 ms until only the leader remains (bounded to ~200 ms). The
  runner's lifeline teardown SIGKILLs the other members pid by pid until
  it is alone.

New tests and red-before evidence (run against the `prelaunch-polish`
manager sources with the new tests):

| Test | On the old code |
|---|---|
| `protocol::t12_restart_after_crash_orphans_the_task` | red: `task … survived the daemon's SIGKILL` |
| `lifecycle_adversarial::d4_daemon_kill9_takes_every_task_down` | red (fixture: a finished task's leader is gone, nothing guards its leftover; after that, the tasks survive the SIGKILL) |
| `lifecycle_adversarial::d4c_daemon_crash_takes_every_task_down` | test-clock only (`debug_crash`, exit 101 = a panic in the main future) |
| `lifecycle_adversarial::d4b_startup_orphans_leftover_records_without_signalling` | red: the record comes back `running` (re-adopted a bystander pid) |
| `mutation_gaps::g6_crashed_task_output_survives_on_the_orphaned_record` | red: `running`, not `orphaned` |
| `mutation_gaps::g14_guardian_runner_does_not_spin` | red: no runner guards the leftover |
| `observability::p2_end_reasons_across_manager_lifecycle` | red: `manager-crash` missing |
| `lifecycle_adversarial::r1_runner_reports_the_commands_real_status` | green (contract continuity); red under ablation `runner-reports-status` |
| eval `(e2) a manager crash ends a backgrounded command with an orphaned exit wake` | red without the extension's reconnect wake: `expected one exit wake` |

Ablations for this branch (manual clock): all 16 new or changed entries
behave as declared (red).

| Ablation | Tests red |
|---|---|
| lifeline-teardown | 3/3: d4, t12, p2 (after t12/p2 learned to check the command's own pid, see below) |
| lifeline-sigkill-after-grace | 1/1: d4 |
| lifeline-read-end-to-runner | 1/1: d4 |
| runner-reports-status | 1/1: r1 |
| runner-guards-leftovers | 2/2: t6d, d4 |
| guardian-poll-sleep | 1/1: g14 |
| runner-blocks-sigterm | 1/1: t5 |
| runner-fds-cloexec | 1/1: t14 |
| crash-orphan-mark | 2/2: p2, d4b |
| lingering-group-tracking (now the `Guarded` arm) | 3/3: t6b, t6c, t6d |
| setsid-process-group (now in `child_setup`) | 4/4: t6, t5, d5, d4 |
| timeout-hard-kill (new exit-watch select) | 1/1: t3 |

After the teardown rewrite (fork-race fix), `lifeline-teardown` first came
back **1/3 red**: `t12` and `p2` only watched the task's pid, which is now
the runner's, and a runner that simply exits on EOF passed them while
`sh` kept running. Both now make the command print its own pid (`echo $$;
exec sleep …`) and require it gone too; the entry is 3/3 red again.

## In-place upgrade (manager-exec-handover)

These tests cover compatible upgrades under the same binary name and home. The initial name transition instead requires finishing or stopping work, closing the previous installation's sessions, waiting for its daemon to exit, and reinstalling. Migrate configuration only, not the runtime state/history tree: records contain absolute output/transcript paths. See [the installation notes](../README.md#install).

Principle: an upgrade is invisible to running work. `pi-famulus upgrade`
(or replacing the binary file) makes the daemon `exec()` the new binary:
same pid, so every runner is still its child; the listener, the daemon
lock, both lifeline ends and every task's stdout / stderr / status pipe are
inherited. Everything else goes through `handover.json`. See
`src/handover.rs` and design §3.2 / §3.3.

State-table cells added by this branch (black box, `tests/upgrade.rs`;
every daemon runs from a private copy of the binary so a test can replace
it):

| # | State | Event | Next state | Side effects | Test |
|---|---|---|---|---|---|
| D19 | serving | `upgrade` (CLI) | serving, same pid, generation + 1 | preflight, then quiesce (pumps park between reads, fanout drains, in-flight requests dropped unanswered, connections closed, writers flushed), exec, restore; every task keeps running | `u1`, `u9`, `u11` |
| D20 | serving | the file at the daemon's path is replaced and settles | as D19, trigger `binary-changed` | polled every 2 s (dev, inode, size, mtime) | `u7` |
| D21 | serving | upgrade to a binary that fails `__handover-check` (broken, non-executable, other format) | serving, unchanged | nothing touched: connections, streams and tasks carry on; `last_upgrade.ok = false` | `u4` |
| D22 | quiescing | exec fails | serving (old image) | descriptors back to close-on-exec, tasks resume with no byte lost, clients reconnect | `u5` |
| D23 | restoring | the new image cannot restore | exited | the lifeline closes: every task and grandchild cleaned up (= a crash, no recovery) | `u6` |
| D24 | serving, just restored | no client for longer than the 5 s idle grace | serving | the idle rule is held for the 30 s handover grace, then applies again | `u12` |
| D26 | serving, protocol < 3 (a manager from before in-place upgrade) | `upgrade` (CLI) | serving, unchanged | the CLI sends no `upgrade`; it names the pid, version and protocol and says to restart once (`pi-famulus shutdown`) | `u13` |
| D27 | serving | `upgrade` from a binary other than the daemon's (e.g. target/release against the installed copy) | as D19 | before asking, the CLI notes on stderr that the daemon execs the file at its own path and names this CLI's path and build; nothing to note from the daemon's own file | `u14` |
| S7 | connected | the manager upgrades | reconnects to the same pid | in-flight requests unanswered (resent by the client); `start` resent with its `key` returns the task it already started; protocol 1 / no-protocol hellos accepted | `u1`, `u2`, `u3` |
| T17 | running / stop grace pending / timeout armed | upgrade | unchanged | later exits report the real code and signal; the timeout fires from the original start; a pending kill grace is re-armed with the time it had left | `u1`, `u8` |
| T18 | running, watched (monitor) | upgrade | unchanged | the session's watch comes back on re-hello with exactly the bytes its connection had not been written; a UTF-8 character split across the handover arrives whole | `u1`, `u10`, `u11` |

**Coverage:** 68 cells (C 10, S 7, T 22, D 29).

Measured client-visible gap (from "quiescing" to the new image accepting,
in `manager.log`, under the parallel suite): 30–46 ms. Connects during it
wait in the listener backlog; none is refused.

Bugs found while building it (each red in a test first):

| Bug | Root cause | Fix | Red → green |
|---|---|---|---|
| requests silently dropped after a rolled-back exec | `watch::Sender::send` does not store the value when no receiver exists; at rollback every parked task had dropped its receiver, so the park flag stayed `true` and every later request was cancelled as parked | `send_replace` | `u5` hung (`response timed out`), green after |
| kill grace gone after an upgrade (manual clock) | the SIGKILL due time was wall time; on the manual clock the grace runs in virtual time, and a >2 s (wall) upgrade left 0 ms | due time in the clock's units (`Clock::now_ms`); the handover carries the manual clock's time | `u8` 5/6 red under the parallel test-clock suite, 6/6 green after |
| a split UTF-8 character became two U+FFFD | the fanout's held-back tail died with the parked fanout | a (re)started fanout seeds its carry from the file (`[delivered, total)`) | `u10` red (`a\u{FFFD}\u{FFFD}b`), green after |
| monitor lines lost at an upgrade | frames queued for a client that was not reading were counted as delivered; the 2 s flush expired and the exec dropped them | writers record the cursor they actually wrote; the carried watch starts there; writers still blocked after the flush are aborted | `u11` about 1 in 2 red under the parallel suite, 8/8 green after |
| CLI `wait` failed mid-upgrade | its hello / id-resolving snapshot did not retry a dropped connection | same reconnect-and-resend as the request | `u9` red once in about 15 suite runs before, 0 in 20+ after |

Two test lessons: `task_exited` sent while a session is disconnected is not
replayed (the extension's sync covers it), so tests wait with `wait`; and a
test must read its old connection to the end (`wait_closed`) before
merging events, or it drops frames the daemon did deliver.

Ablations for this branch (manual clock): all 13 new or updated entries behave
as declared (`scripts/ablate.sh`, exit 0).

| Ablation | Tests red |
|---|---|
| upgrade-preflight | 1/1: u4 |
| upgrade-park-flag-replace | 1/1: u5 |
| upgrade-watch-carry | 2/2: u1, u11 |
| upgrade-fanout-carry-seed | 1/1: u10 |
| upgrade-grace-rearm | 1/1: u8 |
| upgrade-hold-idle | 1/1: u12 |
| upgrade-binary-watch | 1/1: u7 |
| start-key-idempotent | 1/1: u3 |
| upgrade-written-cursor | green by design (see deferrals) |
| bounded-conn-queue, watch-utf8-carry, oversize-response-fallback, timeout-hard-kill (find strings updated for the refactor) | 1/1 each: c6, o4b, o3b, t3 |

Deferrals:
- deferred: the upgrade replays missed output only to a session that reconnects; a session that never comes back loses nothing but also receives nothing (its pi is gone) | impact: none | trigger: multi-client sessions
- deferred: the `upgrade-written-cursor` ablation is undetectable in isolation (loss needs a client that stops reading with a full queue past the 2 s flush); only the parallel suite / stress catch it | impact: a regression would show as rare missing monitor lines | trigger: a flaky `u11`, or a hook that can stall a connection's reader
- deferred: preflight runs the new binary once (up to 20 s on a first run under macOS signature assessment) while everything is still served; an upgrade right after `install` can take that long | impact: `pi-famulus upgrade` waits; nothing is interrupted | trigger: user reports of slow upgrades
