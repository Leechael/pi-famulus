# pi-famulus CLI

Standalone operations manual for the `pi-famulus` binary.

The pi extension talks to the daemon over a socket. These subcommands are the human and scripting surface for inspection, debugging, and smoke tests. Human-readable tables are **not** a wire contract; use `--json` (on `status`, `sessions`, `ls`, `show`, `stats`, `events`) for scripts.

From the CLI alone you can answer: what is each session doing and where (cwd), what is running or just finished, why did it end, what did this subagent do, why didn't a notification arrive, and is the system healthy.

## Which command answers which question

| Question | Command |
|---|---|
| Is the daemon healthy? | `pi-famulus doctor` (non-zero exit on any failure), `pi-famulus status` |
| What is each pi session doing, and where? | `pi-famulus sessions` (connected sessions: PID, CWD, running/tasks/agents) |
| What is running / just finished? | `pi-famulus ls [-a] [--session P] [--cwd DIR] [--since 10m] [--json]` (what is running, newest first; `-a` adds connected sessions' finished work; KIND, CWD, STATUS, DUR, EXIT, REASON). A session that exits drops its finished rows from `ls` at once, and drops out of `sessions` too once nothing of it is still running; either way its running work stays listed until it ends, and its agent records, transcripts and events stay reachable for `goneSessionRetention` (default 24h); a finished task's own record can expire sooner under `finishedTaskRetention` |
| Why did this end? What did it print? | `pi-famulus show <id>` (shell, monitor, `ch_…` agent or `run_…`) |
| What did this subagent do? | `pi-famulus agent <ch_id> [-f] [--full]` (live transcript) |
| Which subagent spent how much CPU on which kind of task? | `pi-famulus stats [--by agent\|kind\|agent,kind] [--since 2h]` (tasks, wall, CPU, average cores, unmeasured, killed); `ls` shows CPU and CORES per finished task |
| Why didn't a notification arrive? | `pi-famulus events [-f] [--id X]` (task lifecycle + wake emit/deliver/inject/dedupe/drop; `wake.inject lag_ms` = how long a wake waited before the model saw it) |
| Follow output | `pi-famulus tail <id>`, `pi-famulus log -f <id> [--stderr]` |

Ids are fuzzy (unique prefix/suffix/near-miss). State directory: `~/.pi/agent/pi-famulus/` (`PI_FAMULUS_HOME` / `--home`).

## Install

Install `pi-famulus` with optional dependencies enabled (`pi install npm:pi-famulus`). The main package automatically selects the matching Linux/macOS x64/arm64 or Windows x64 native package and exposes the CLI on its npm bin path; `npx pi-famulus --help` is a quick check. No compiler or separate binary download is needed. See the [README](../README.md#install).

### Source build / separate manager

```bash
cd manager && cargo build --release
mkdir -p ~/.pi/agent/pi-famulus/bin
# Use `install` (or cp→mv) so the path gets a new inode. Overwriting the
# existing file in place invalidates macOS's code-signing cache and the next
# exec is SIGKILL'd (`killed`, exit 137) even when `codesign -vv` still says valid.
install -m 755 target/release/pi-famulus ~/.pi/agent/pi-famulus/bin/pi-famulus
export PATH="$HOME/.pi/agent/pi-famulus/bin:$PATH"
```

If you already hit `killed` after a reinstall, fix with another atomic replace (same `install` line above), or `cp …/pi-famulus …/pi-famulus.new && mv …/pi-famulus.new …/pi-famulus`.

**One-time name transition:** switching installation names is a breaking change, not an in-place upgrade. Wait for work to finish or stop it, close the sessions using the previous installation, and wait for its daemon to exit. Reinstall at the paths above and migrate **configuration only** to `~/.pi/agent/pi-famulus/config.json`, updating explicit paths and environment overrides, then reopen sessions. Do not move the runtime state/history tree: records contain absolute output/transcript paths, and moving a directory does not rewrite them. Keep previous history separately if needed.

**Subsequent same-name upgrades while pi sessions run work:** just `install` the new binary. A running daemon notices within a few seconds and upgrades itself in place (see [`upgrade`](#upgrade)); `pi-famulus upgrade` does it now and reports the result. Nothing running is interrupted and no pi session needs a restart; reload or reopen pi sessions only when you also want the new extension code. This applies to compatible builds under the same name and home, not the one-time transition above.

The extension prioritizes executable config `managerPath`, executable `PI_FAMULUS_MANAGER_PATH`, the exact-version native npm package, this home/bin path, then executable PATH. The npm CLI always runs its own native package. Restart sessions when changing installation method/binary location; the daemon's in-place monitor follows its current path, not a different package's executable.

## Global options

| Flag / env | Meaning |
|---|---|
| `--home <dir>` | State directory for this invocation |
| `PI_FAMULUS_HOME` | Same, if `--home` is omitted |
| (default) | `~/.pi/agent/pi-famulus` |
| `--no-pager` | Never page output (see below) |
| `PI_FAMULUS_PAGER`, then `PAGER` | Pager for listings on a terminal; `cat` = none, empty = falls through |

Priority: `--home` > `PI_FAMULUS_HOME` > default.

**Pager.** When stdout is a terminal, `sessions`, `ls`, `show`, and `agent` / `events` / `log` without `-f` go through a pager, like git: `PI_FAMULUS_PAGER`, else `PAGER`, else `less`. An empty value is treated as unset and falls through to the next choice, so `PI_FAMULUS_PAGER=` lets `PAGER` take over. A bare `less` runs as `less -FRX`, on top of any `LESS` you set, so output that fits one screen prints and returns at once; a pager given with its own arguments runs as given. `cat` (from either variable) disables paging outright. Piped or redirected output is never paged, so scripts see plain text.

Layout under home:

```
manager.sock
manager.pid
manager.lock           # held by the running daemon for its whole lifetime
manager.spawn.lock     # held by a client while it spawns the daemon
manager.log
events.jsonl           # daemon.start / daemon.shutdown
config.json            # optional
sessions/<session_id>/events.jsonl
sessions/<session_id>/tasks/<task_id>.{json,output,stderr}
sessions/<session_id>/agents/<child_id>.{json,jsonl}   # written by the extension
```

## Quick reference

```bash
pi-famulus status [--json]
pi-famulus sessions [--json]
pi-famulus ls [--session PREFIX] [--cwd DIR] [--since DUR] [--json]   # alias of list
pi-famulus show <id> [--json]
pi-famulus top [--json]
pi-famulus stats [--by agent|kind|agent,kind] [--session PREFIX] [--cwd DIR] [--since DUR] [--json]
pi-famulus agent <ch_id> [--full] [-f]
pi-famulus events [-f] [--session PREFIX] [--id ID] [--since DUR] [--json]
pi-famulus log [-f] [-n 100] [ID] [--stderr]
pi-famulus tail <ID> [-n 100] [--stderr]
pi-famulus output <id> [-f] [--max-bytes N]
pi-famulus wait <id> [--budget-ms 20000]
pi-famulus completion --shell <bash|zsh|fish>
pi-famulus config get max-agents
pi-famulus config set max-agents <count>
pi-famulus stop <id>
pi-famulus kill-session <session_id>
pi-famulus start [--session cli] [--kind shell|monitor] [--cwd DIR] [--timeout-ms N] [--background] '<cmd>'
pi-famulus doctor
pi-famulus shutdown
pi-famulus daemon [--foreground]
```

Ids: `sh_…` shell, `mon_…` monitor, `ch_…` agent (subagent child), `run_…` subagent run. Every command that takes an id accepts it **fuzzily**: exact, a unique prefix/suffix/substring (`e1351cb1`, `mon_e135`), or a unique near-miss within edit distance 2 (`cmon_…` → `mon_…`); a fuzzy match prints `note: resolved '…' → '…'` on stderr. Ambiguous input lists up to five candidates. An unknown id prints one line with the closest known id: `unknown id 'x' (did you mean 'y'?)`.

Durations (`--since`): `500ms`, `30s`, `10m`, `2h`, `1d` (a bare number is seconds).

### Which commands start the daemon

| Starts the daemon when none runs | Never starts it |
|---|---|
| `ls`, `output`, `wait`, `stop`, `kill-session`, `start` | `status` (stderr: `pi-famulus: pi-famulus is not running`, exit 1), `sessions`, `show` and `stats` (read the disk instead), `agent`, `events`, `log`, `tail`, `completion`, `config`, `doctor`, `shutdown` (stdout: `pi-famulus is not running`, exit 0) |

A daemon started this way exits again ~5s after its last client leaves (§3.2).

Output is pipe-friendly: when the reader goes away (`… | head`), the CLI exits quietly with status 0.

### Shell completions

Generate a dynamic completion script for bash, zsh, or fish with `pi-famulus completion --shell <shell>`. Install the returned script using the convention for your shell; keep `pi-famulus` on `PATH` because the script asks it for candidates as you type.

---

## Inspection

### `status`

```text
version:  0.1.0+066598ae00 (protocol 4)
pid:      4321
binary:   /Users/me/.pi/agent/pi-famulus/bin/pi-famulus
uptime:   13m23s
agent slots: 1/8 used
sessions: 2 (1 connected)
tasks:    3 running, 8 finished (shells 2/5, agents 1/3)
agent tokens: 8200 input / 460 output (cache read 6100 / write 280)
```

The version carries the commit the binary was built from, so two builds of 0.1.0 differ; `unknown` for a build outside a git checkout. `binary` is the daemon's file, the one an [`upgrade`](#upgrade) execs, which is not necessarily the CLI you ran. Counts include agents (running/finished shells and agents are also shown separately). `agent tokens` sums provider-reported cumulative child usage. If a retained agent record lacks a counter (as in older records), that aggregate is unavailable rather than treated as zero. `input` is exactly provider `usage.input`; `cache read` and `cache write` are separate counters (`usage.cacheRead` / `usage.cacheWrite`), not included in input. Records refresh as child messages finish, and the extension also appends absolute totals in `agent.usage` events for readers of the shared event stream. Human-readable `output tok/s` is cumulative output tokens divided by observed LLM message-in-flight milliseconds; it is omitted until an LLM interval is observed. In `--json`, `agent_tokens.output_tokens_per_second` is `null` until then; cache counters are `tokens_cache_read` and `tokens_cache_write`. `--json` prints the protocol `status` response plus `agent_counts` and `agent_tokens`. With no daemon: `pi-famulus: pi-famulus is not running` on stderr, exit 1 (also with `--json`).

### `sessions`

```text
SESSION   PI_PID STATE     CWD         SINCE    LAST_SEEN RUNNING TASKS AGENTS
0199aaaa  81234  connected ~/src/app   14:02:11 now       2       7     1
```

Connected sessions only (a gone session is listed while it still runs something). When a pi session exits, it leaves the listings at once; its files stay on disk for `goneSessionRetention` (see below) so `agent` and `events --session` still reach it, and are then deleted. A finished task's own record and output reach `show` for only `finishedTaskRetention` (see below), which can be shorter. `SESSION` is the shortest unique prefix, at least 8 characters. `RUNNING` counts running tasks and agents; a record that says an agent is running while its session is gone is not counted (it cannot be alive).

### `ls` / `list`

```text
$ pi-famulus ls
ID           KIND    SESSION   CWD        STATUS    TIME     DUR    CPU   CORES NOW  SAMPLE   EXIT    REASON       TITLE
sh_3f2a91c0  shell   0199aaaa  ~/src/app  running   14:03:22 1m04s  -     -     -    -        -       -            npm test
ch_9a41c7e2  agent   0199aaaa  ~/src/app  running   14:02:50 3m10s  -     -     -    -        -       -            review (worker) m1

$ pi-famulus ls --all
ID           KIND    SESSION   CWD        STATUS    TIME     DUR    CPU   CORES NOW  SAMPLE   EXIT    REASON       TITLE
sh_3f2a91c0  shell   0199aaaa  ~/src/app  running   14:03:22 1m04s  2.1s  0.0   0%   14:04:18 -       -            npm test
ch_9a41c7e2  agent   0199aaaa  ~/src/app  running   14:02:50 3m10s  -     -     -    -        -       -            review (worker) m1
sh_51d0c3aa  shell   0199aaaa  ~/src/app  completed 14:01:40 41s    2m28s 3.6   -    -        0       exited       cargo test
mon_e1351cb1 monitor 0199aaaa  ~/src/app  killed    14:01:10 30s    0.0s  0.0   -    -        SIGTERM  stopped:tui  tail -f log
ch_7d0e22a1  agent   0199aaaa  ~/src/app  failed    14:00:05 12s    -     -     -    -        -       -            model-error  broken (worker) m1
```

By default only running work, anywhere (a live process is never hidden, even in a gone session). `-a`/`--all` adds the finished work of connected sessions; a gone session's finished work is reached by id (`show`) until its session's retention or the finished-task retention ends, whichever comes first. Running rows come first, then finished ones, each newest first. `TIME` is a task's start and an agent's **last transcript message** (a long-running agent that just spoke sorts as recent; its start is in `show`). Filters: `--session PREFIX` (session id prefix), `--cwd DIR` (that directory or below; agents use their session's cwd), `--since DUR` (TIME within). `--json` prints the rows in the same order as an array of objects (`id`, `kind`, `session_id`, `cwd`, `status`, `started_at`, `active_at` (= TIME), `ended_at`, `duration_ms`, `exit_code`, `signal`, `end_reason`, `title`, `running`, plus `pid`/`origin`/`backgrounded_at`/`run_id`/`error` when known, and a task's `work_kind`).

- `CPU` is a finished task's user + system CPU time. For a running task it is the latest best-effort cumulative process-group sample; `CORES` is lifetime average when finished and the sample's lifetime average while running. `NOW` is CPU used during the most recent ~5 s sample interval as percent of one core (a process group using several cores can exceed 100%). `SAMPLE` is the last successful sample time; `NOW=stale` means the latest sampling attempt failed, so no live percentage is reported. Live sampling reads visible Linux `/proc` group members, is response-only (not persisted), and can miss descendants that exited between samples; it is unavailable on platforms without `/proc`. Finished measurements cover the command and every descendant waited for by its parent (pytest's xdist workers, a compiler under make), measured when the command exits. `-` means no sample/report yet, for agents, or when usage was not measured: a hard timeout (`--timeout-ms`) or stop past its 2s grace SIGKILLs the runner with the group. Never counted: processes that escaped the wait chain (`setsid`, `cmd &` never waited for, a worker orphaned because its parent died first). On macOS, a process that reaps children and then `exec`s loses their CPU (`make; exec foo` shows only `foo`). `--json` carries `cpu_user_ms`, `cpu_sys_ms`, `max_rss_kb` and (running tasks only) `live_cpu_user_ms`, `live_cpu_sys_ms`, `live_cpu_percent`, `live_cpu_sampled_at`, and `live_cpu_stale`.
- `EXIT` is the exit code, a signal name (`SIGTERM`, `SIGKILL`, …), or `-`.
- `REASON` is the task's `end_reason` (see below), or an agent record's `end_reason`.
- `TITLE` is the command's first line (agents: `name (agent) model`), truncated by **display width** so CJK and emoji keep the table aligned: to the terminal width on a tty, to 60 columns otherwise.

`end_reason` values: `exited` (the process exited on its own, any code) · `timeout` (`timeout_ms` ceiling or a stop with reason timeout) · `stopped:tui` / `stopped:cli` / `stopped:tool` (a stop request, by who) · `rate-limit` · `session-end` · `manager-shutdown` · `manager-crash` (the manager died without shutting down, e.g. `kill -9`; its task was taken down with it, and the next daemon marked the record `orphaned`).

`work_kind` (a task's, in `--json` and `show`) is a guess from the command text: `test-suite`, `test`, `build`, `lint/type`, `git`, `read/search` or `other`; monitors are `monitor`. Agents write compound commands, so the heaviest simple command wins (test-suite > test > build > lint/type > other > git > read/search): `cd x && pdm run test > log 2>&1; tail -n 100 log` is a `test-suite`, not a read. `sh/bash -c '…'` is looked into, heredoc bodies are not, and wrappers (`pdm run`, `uv run`, `npx`, `xargs`, `env`, `timeout`, `python -m`) are peeled. A test runner with no target is a whole suite; a path, a node id, `-k`/`-m`/`--lf`, a `$` expansion, or targets fed by `xargs` make it `test`. It is computed when read, never stored, so a better rule also applies to old records. Runner options and their values (`npm --prefix web test`) are skipped, and selection flags count in any spelling (`-kauth`, `--test=name`). `npm test -- --run` remains a whole-suite run because `--run` selects one-shot mode, not tests. `pdm run py-compile`-style scripts count as `build` and `awk` as `read/search`, by choice. Project-specific script names that say nothing (`pdm run go`) land in `other`. Per-project command overrides are **not supported**.

### `show`

Everything about one id, any kind:

- **task / monitor:** status, exit, reason, session (state, pi pid), full command, cwd, pid, start/end/duration, when it was moved to the background, who spawned it (`origin`: `bash-fg`, `bash-bg`, `child-bash` with child and run, `monitor`), output and stderr paths, wake notification emitted → delivered (from the extension's events), its `work_kind`, CPU (user/sys, average cores, peak RSS, or why it was not measured), and the last 10 output lines.
- **agent (`ch_…`):** name/agent/model, run, status and end reason, error, start/end/duration, tool-call count, shells it spawned (tasks whose `origin.child_id` is this agent), cumulative provider `usage.input`/`usage.output` tokens and separate cache-read/cache-write counters, observed wall split, transcript path, the task prompt, and the last 20 lines of its result. Cached tokens are not folded into input. The split attributes assistant message-in-flight intervals to LLM, `tool_execution_start`..`tool_execution_end` intervals to tools, and admission wait to queue; unmatched or residual time is `unclassified` and sets `wall_approximate`. `output tok/s` = output tokens / LLM milliseconds × 1000; it is cumulative over observed LLM intervals, not wall time.
- **run (`run_…`):** its children as an `ls` table.

`--json` prints the underlying records, the output tail, and the related events.

### `top`

A plain-text snapshot of retained task CPU grouped by agent and work kind, together with child token totals and wall attribution:

```text
$ pi-famulus top
pi-famulus top — CPU totals are per-agent task CPU, including monitors; NOW is the latest sampled CPU rate (100% = one core).
AGENTS
  worker (ch_1234) | CPU 12s total, 85.0% now (sample 14:04:23) | 2 task(s) | tokens 8200 in / 460 out (cache read 6100 / write 280), 24.0 output tok/s | wall LLM 19s / tool 3s / queue 500ms / unclassified 20ms
WORK KINDS
  test-suite | 1 task(s) | CPU 10s total, 85.0% now (sample 14:04:23)
```

`CPU total` combines final runner CPU from ended tasks (including monitors) with daemon live process-group estimates for running tasks; shell tasks attributed to a child use `origin.child_id`, while monitor tasks without a child id appear under `main <session>`. It is not a measurement of remote model compute or agent runtime overhead. `NOW` is the recent sampling interval, which refreshes about every 5s; percentages can exceed 100% when multiple cores are used. A `NOW` value of `unavailable` (JSON `cpu_now_stale: true`) distinguishes a failed sample from no sample yet; `sample` is the most recent successful timestamp (`cpu_sampled_at` in JSON). Agents with usage but no shell tasks still appear. `tokens_input` is exactly provider `usage.input`; cache-read (`tokens_cache_read`) and cache-write (`tokens_cache_write`) counts are separate and not included in input. `output tok/s` is cumulative provider output tokens divided by observed assistant-message LLM milliseconds × 1000, not total elapsed time; it is unavailable when either value is missing. Wall split classifies observed LLM message, tool execution, and admission queue intervals; unclassified time is approximate and is not assigned to a phase. `--json` returns `agents[]` and `work_kinds[]` with millisecond totals, separate cache counters, and optional CPU rates plus freshness fields. This command takes a snapshot; it does not refresh continuously and never starts the daemon.

### `stats`

Where retained task wall time and CPU went, grouped by the subagent that ran each task, by the task's work kind, or both:

```text
$ pi-famulus stats --by agent,kind
AGENT                     KIND        TASKS   WALL     CPU  CORES  UNMEASURED  KILLED  KILLED-WALL
wave2-kms (ch_1f1f7f0e)   test-suite      4  1h06m  3h41m    3.3           1       1       20m14s
wave2-kms (ch_1f1f7f0e)   lint/type       3  6m10s  4m02s    0.7           0       0          0ms
main 01a10bda             git            39  1m02s   3.1s    0.1           0       0          0ms
TOTAL                                    46  1h13m  3h45m    3.0           1       1       20m14s
```

- **AGENT** is the subagent in the task's `origin.child_id`, named from its agent record (`name (ch_…)`; the bare id when the record is gone). Tasks a session's main agent ran itself (including monitors without a child id) are `main <session>`.
- **KIND** is the task's `work_kind` (see `ls`): `test-suite`, `test`, `build`, `lint/type`, `git`, `read/search`, `other`, or `monitor`. Not the shell/monitor `KIND` of `ls`. `npm test -- --run` is still a whole suite (`--run` selects one-shot mode, not tests); a path or test-name filter makes it `test`.
- **WALL** sums the tasks' durations (a running task's so far). **CPU** sums user + system CPU for tasks with a final measurement or a live process-group estimate, and **CORES** divides it by the wall time of those same tasks. **UNMEASURED** counts tasks without an available final/live measurement (e.g. SIGKILLed with their runner by `--timeout-ms` or after a stop's grace, or recorded by an older manager); `-` means no task in the row was measured. Live CPU is best-effort and sampled about every 5s; see `ls` for what CPU covers.
- **KILLED** / **KILLED-WALL**: tasks that ended `killed` and the wall time they ran before that.

Every retained task record counts, including finished work of gone sessions (`ls` leaves those out); rows are sorted by CPU, then wall time, with a `TOTAL` row last. Filters as in `ls`: `--session PREFIX`, `--cwd DIR`, `--since DUR` (tasks running at some point within it). Per-project command overrides are **not supported**; commands use the built-in classifier, so opaque scripts can land in `other`. `npm test -- --run` is classified as a whole suite (`--run` chooses one-shot mode, not test selection). `--json` prints the groups with raw milliseconds (`wall_ms`, `cpu_user_ms`, `cpu_sys_ms`, `cpu_ms`, `measured`, `measured_wall_ms`, optional `cpu_now_percent`, `cpu_now_stale`, `cpu_sampled_at`, `killed`, `killed_wall_ms`, and `avg_cores` when measured wall time is nonzero). Never starts the daemon.

### `agent`

```bash
pi-famulus agent ch_7d0e22a1          # conversation, preamble hidden
pi-famulus agent ch_7d0e22a1 --full   # include system prompt / agent preamble
pi-famulus agent ch_7d0e22a1 -f       # keep following
```

Renders the transcript `sessions/<sid>/agents/<ch>.jsonl` (one JSON object per message: `role`, `text`, `tool`, `args`, `isError`, `ts`). Without `--full`, `system` messages and lines marked `"preamble":true` are hidden, and the first user message is shown as the task prompt (`prompt_head` from the agent record).

### `events`

```text
2026-09-23 14:03:22.123 0199aaaa manager   task.exit          sh_3f2a91c0 cpu_sys_ms=9214 cpu_user_ms=201330 duration_ms=64012 end_reason=exited exit_code=0 max_rss_kb=412880
2026-09-23 14:03:22.140 0199aaaa extension wake.emit          - kind=task ids=["sh_3f2a91c0"] batch=1
2026-09-23 14:03:22.140 0199aaaa extension wake.deliver       - kind=task mode=steer
2026-09-23 14:03:31.502 0199aaaa extension wake.inject        - kind=task ids=["sh_3f2a91c0"] as_of=1790143402140 lag_ms=9362
```

`wake.deliver` is when the extension handed the wake to pi; `wake.inject` is when it entered the model's context, and `lag_ms` how long it waited in between (a steered wake waits for the parent's current tool calls and turn). Passive wakes (`mode=passive`) have no `wake.inject`: pi appends them when they are sent.

The event log, merged and time-ordered across every session (plus daemon events). Filters: `--session PREFIX`, `--id ID` (matches `id`, `child_id`, or an entry of `ids`), `--since DUR`. `--json` prints one raw event per line with a `session` field added. `-f` follows all files, including sessions that appear later. Malformed lines are skipped, with a count on stderr.

What is logged: see design §3.3 "Event log". The manager writes `session.connect/disconnect`, `task.start/background/stop/exit`, `daemon.start/shutdown`; the extension writes wake, monitor, agent and decision events.

### `log` / `tail`

```bash
pi-famulus log                    # last 100 lines of manager.log, local timestamps
pi-famulus log -f
pi-famulus log sh_a1b2c3d4        # task's merged stdout+stderr
pi-famulus log -f sh_a1b2c3d4 --stderr
pi-famulus log ch_7d0e22a1        # agent: rendered transcript
pi-famulus tail sh_a1b2c3d4       # = log -f; -f is accepted and ignored
```

| Flag | Meaning |
|---|---|
| `-f` / `--follow` | Keep reading new bytes (`tail` always follows) |
| `-n` / `--lines` | Trailing lines before follow (default 100) |
| `--stderr` | `<task>.stderr` instead of the merged `.output` (tasks only) |

`log`/`tail` read files directly (polling ~200ms); `output` goes through the protocol:

| | `log` / `tail` | `output` |
|---|---|---|
| Source | On-disk `.output` / `.stderr`, agent transcript | Protocol cursor (ring + file), agent result |
| Best for | Watching by hand | Scripts matching extension semantics |
| Starts the daemon | No | Yes |

### `doctor`

Health checks, one line each (`ok`, `fixed`, `warn`, `FAIL`), then `ok` or `N problem(s) found`. **Exit status 1 when any check fails.**

- home exists (doctor never creates it)
- socket path length fits a unix socket (103 bytes on macOS, 107 on Linux)
- `config.json` parses; the manager path from `PI_FAMULUS_MANAGER_PATH` or `managerPath` exists
- daemon: running exactly when it holds `manager.lock` (the recorded pid is not trusted; it may belong to another process after a crash). Running → probe the socket. Not running → take the lock and remove stale socket/pid files (`fixed`, not a failure)
- protocol: every connected session announced the manager's protocol
- stale agent records: an agent says running but its session is gone
- orphan pids: a task still running with no manager to own it
- session retention: `goneSessionRetention` in `config.json` is a valid duration
- task retention: `finishedTaskRetention` in `config.json` is a valid duration
- sessions dir size (warn above 100 MiB; events.jsonl has no rotation yet)

---

## Acting on tasks

### `output`

```bash
pi-famulus output sh_a1b2c3d4
pi-famulus output sh_a1b2c3d4 -f              # follow until finished and caught up
pi-famulus output sh_a1b2c3d4 --max-bytes 4096
```

The same byte stream the extension sees (§3.3 cursor reads; chunks never split a UTF-8 character). `--max-bytes N` prints **at most N bytes in total**; a character that would cross the limit is left out. For an agent id, prints the agent's result.

### `wait`

```bash
pi-famulus wait sh_a1b2c3d4 --budget-ms 5000
```

| Outcome | Printed line |
|---|---|
| Exited | `done exit_code=N` / `done exit_code=null` |
| Budget expired | `not done (budget expired; task still running)` |
| Agent finished | `done status=completed` |

Exit status 0 in every case unless the request fails. The task keeps running after the budget expires.

### `stop`

Stop one task: SIGTERM to its process group → 2s → SIGKILL (§3.3), recorded as `stopped:cli`. Prints `stopped <id>`, or `<id> already finished (<reason>)` for a task that had already ended (leftover background children of a finished task are still cleaned up). For an agent id: `agents run inside pi; stop from /tasks or ask the agent` (exit 1): the CLI cannot reach in-process children.

### `kill-session`

Stops every running task of a session (list + stop, reason `cli`).

### `start`

Convenience spawn for scripting and smoke tests. Binds the task to an extension-style session (default `cli`).

```bash
pi-famulus start 'echo hello'
pi-famulus start --session my-sess --cwd /tmp --timeout-ms 60000 'sleep 5 && echo done'
pi-famulus start --kind monitor --background 'while true; do date; sleep 1; done'
```

| Flag | Default | Notes |
|---|---|---|
| `--session` | `cli` | Owning session id |
| `--kind` | `shell` | `shell` or `monitor` |
| `--cwd` | process cwd | Working directory |
| `--timeout-ms` | none | Hard kill ceiling (`end_reason: timeout`) |
| `--background` | off | Semantic marker only |
| `<COMMAND>` | required | Run via `sh -c` (Windows: pi's bash when available, else `cmd.exe /d /s /c`; see design §3 rulings) |

Prints `task_id=sh_… pid=12345`.

---

## Daemon

### `config`

Set or read runtime settings. `max-agents` is the machine-wide concurrent subagent budget (default 8). Protocol 5 also supports per-kind keys `max-test-suite`, `max-test`, `max-build`, `max-lint/type`, `max-other`, `max-git`, and `max-read/search`; unset kinds inherit the current global limit except test/test-suite, which default to 2. Changes persist in `config.json`, take effect without restarting the daemon, and re-evaluate queued admissions. See [Machine-wide agent capacity](global-capacity.md) for the field map and scheduling behavior.

```bash
pi-famulus config get max-agents
pi-famulus config set max-agents 12
pi-famulus config set max-test-suite 3
pi-famulus config get max-test-suite
```

### `daemon`

Runs the manager in the foreground (what auto-spawn uses); `--foreground` also logs to stderr. One daemon per home: the daemon holds `manager.lock` for its lifetime; a second one prints "already running" and exits 0.

### `upgrade`

Replaces the running daemon, in place, with the binary now installed at its path: same pid, every task keeps running (and later reports its real exit code), clients reconnect by themselves within tens of milliseconds. Requests in flight during the switch are resent by the clients; a `start` is never run twice.

```text
upgraded in place: 0.1.0 -> 0.1.1 (pid 4321, generation 1, 3 running task(s) kept)
```

- The daemon execs the file at **its own path** (`binary:` in `status`), not the CLI you run. Running `upgrade` from another file (say a fresh `target/release`) prints a note saying so; install the build to that path first.
- The new binary is checked first (`__handover-check`). A missing, broken or incompatible binary stops the upgrade before anything is touched: `upgrade not done, still running 0.1.0: …` (exit 1).
- If the switch itself cannot finish (quiesce over 5s, exec failure), the daemon keeps running the old binary and says why.
- If the new binary cannot restore, it exits and every task is cleaned up, as in a crash (no crash recovery); `upgrade` reports `the manager (pid N) exited during the upgrade`.
- With no daemon running: `pi-famulus is not running; the next client starts the installed binary` (exit 0).
- The daemon does the same by itself when the file at its path changes and settles (checked every 2s). `status` then shows `upgrades: 2 (last: 0.1.0 -> 0.1.1, binary-changed, 3m ago)`, or `upgrades: 1 (last attempt failed 2m ago, cli: …)`; nothing while there has been no upgrade. `status --json` has `generation` and `last_upgrade` (`trigger: "cli"` or `"binary-changed"`).

### `shutdown`

Asks the daemon to shut down gracefully: every running task and every leftover process group of a finished task gets SIGTERM, then SIGKILL after 2s; records end as `manager-shutdown`. Prints `manager shutting down` on stdout; with no daemon, prints `pi-famulus is not running` on stdout and exits 0. The daemon also shuts itself down ~5s after its last client disconnects.

The manager is the parent of every task and there is no crash recovery. Each task runs under a small runner (`pi-famulus __run`) that holds a lifeline to the daemon. If the daemon dies without shutting down (`kill -9`, a panic), every runner sees the lifeline break and takes its process group down: SIGTERM, then SIGKILL after 2s, background children included. The next daemon re-adopts nothing and signals nothing: only records still persisted as `running` are marked `orphaned` with `end_reason: manager-crash`; a command that already exited (even if its guardian runner is still cleaning up leftover children) has a terminal record that stays unchanged.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success, including `wait` budget expiry, `stop` on a finished task, `shutdown` with no daemon, and a reader closing the pipe |
| `1` | Error (`pi-famulus: …` on stderr), `status` with no daemon, a `doctor` check failed |

## Retention of gone sessions

A session is *gone* once its pi process disconnects. Gone sessions leave `ls` and `sessions` immediately. Their directory `sessions/<sid>/` (task records and output, agent records and transcripts, `events.jsonl`) is kept for `goneSessionRetention` after its last write, then the daemon deletes it and forgets its tasks. The daemon sweeps at startup and every `min(retention, 1h)` (at least every second). A session that is connected, or still owns a running task or a live process group, is never swept.

```json
{ "goneSessionRetention": "24h" }
```

in `<home>/config.json`; any duration (`30m`, `7d`, `0s` = at the next sweep). Default `24h`. An invalid value makes `doctor` fail and the daemon use the default.

## Retention of finished tasks

A connected session is never swept, so a pi session left open for days would keep every command's record and output, and the daemon loads all of them at startup. Independently of the session, a finished task's files (`<id>.json`, `.output`, `.stderr`) are deleted `finishedTaskRetention` after it ended, in every session, and the task leaves `ls` and `show`. A task whose process group still has members is kept until it empties. Agent records and transcripts and `events.jsonl` are not touched by this rule (only by the session retention above).

```json
{ "finishedTaskRetention": "24h" }
```

Same duration format, default and `doctor` check as `goneSessionRetention`. The sweep runs with the session sweep, at the shorter of the two cadences.

## Typical workflows

```bash
# What is running, where, and why did the last thing stop?
pi-famulus ls
pi-famulus ls --since 10m
pi-famulus show e1351cb1

# What did a subagent do?
pi-famulus ls | grep agent
pi-famulus show ch_7d0e22a1
pi-famulus agent ch_7d0e22a1

# Why didn't a notification arrive?
pi-famulus events --id sh_3f2a91c0

# Smoke-start and watch
line=$(pi-famulus start 'for i in 1 2 3; do echo $i; sleep 1; done')
id=${line#task_id=}; id=${id%% *}
pi-famulus tail "$id"

# Health
pi-famulus doctor
```
