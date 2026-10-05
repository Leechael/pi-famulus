# pi-famulus design document

Version: v0.1 (2026-09-17)
Status: Confirmed baseline, entering implementation

## 1. Background and goals

pi (`@earendil-works/pi-coding-agent`) deliberately leaves subagents, background bash, monitoring, and similar capabilities out of core (see usage.md), delegating them to extensions. This repository implements a standalone pi extension + process-management daemon providing four capabilities:

1. **subagents** — Multiple parallel or sequential executions; synchronous waiting with a foreground budget, automatically switching to asynchronous execution when the budget expires
2. **monitoring** — Create monitoring tasks (command-line event streams), with events actively injected
3. **bash auto-backgrounding** — Automatically move commands to the background when the foreground budget expires, avoiding stuck waits and actively notifying on completion
4. **agent communication and collaboration** — ask/reply/send between parent and child subagents (in-process mailbox)

### Confirmed decisions

| Decision | Conclusion |
|---|---|
| Orchestration expression | Tool-parameter style: parallel `tasks[]` + sequential interpolation in `chain[]` (lain style) |
| Package positioning | An entirely new standalone package. Not compatible with pi-subagents; cross-session/cross-machine communication belongs to agent-intercom (a separate system), not this package |
| Synchronous strategy | Smart budget mode: wait synchronously by default, automatically switch to asynchronous execution when the foreground budget expires (grok style) |
| Child-agent process model | v1 uses in-process `createAgentSession` throughout; the execution layer exposes a `ChildRunner` seam for a future detached backend |
| Process management | Standalone Rust binary `pi-famulus`, machine-wide singleton, conventional paths, isolation through session_id namespaces |
| Manager lifecycle | **Lives and dies with pi processes**: zero active connections → kill remaining tasks → exit. Started under a lock when needed. Never resurrects itself; only clients have spawn authority |
| Manager language | Rust (tokio + usage-rs + serde_json + interprocess + fd-lock) |

### Research sources (findings incorporated; no implementation dependency)

- **lain** (local TS): automatic backgrounding after a 15s foreground budget, durable inbox notifications, ordinal ordering, fail_fast stops only unstarted tasks, child-agent shells forbid backgrounding (waitUntilExit), ask uses a unified request_id
- **claude-code 2.1.201**: timeout switches to background rather than killing, `<task-notification>` user-role injection, Monitor tool (200ms batching / 500 per line / 3000 per batch), bare-sleep interception, 10min stall watchdog, sending a message resumes the agent
- **grok-build** (Rust): single-writer coordinator actor, ChildRunner host seam, 45s foreground budget, admission queues rather than rejects, monitor = file tail + rate limiting, three delivery states: steer/queue/interject
- **pi-subagents 0.68** (cautionary example): lessons from 23k lines—detached-runner evidence chains, pruned-fork summaries, workflowScript VM, dual communication channels; copy none of these
- **pi-intercom 0.12.1** (deprecated): borrow only the daemon singleton spawn flow and idle/busy injection modes; its global single-ask lock and zombie lifecycle are cautionary examples

## 2. Overall architecture

```
pi instance A (session a) ──┐
pi instance B (session b) ──┼── conventional-path socket ──► pi-famulus (Rust, machine-wide singleton)
pi instance C (session c) ──┘                                ├─ process engine: spawn/wait/stop/output
                                                            ├─ session_id namespaces
    pi extension (TS, in-process)                           ├─ dual output writes (bounded memory + full disk log)
    ├─ bash override ────► manager client ──────────────────┤─ event push (task_started/exited/output)
    ├─ monitor tool ────► start + watch output stream        └─ lifecycle: zero connections → cleanup and exit
    ├─ task_list/output/stop
    ├─ subagent tool ───► ChildRunner seam (v1: InProcessRunner)
    ├─ comms ──► in-process mailbox (contact_supervisor / agent_message)
    └─ NotifyCenter ──► sole injection outlet (triggerTurn/steer/passive)
```

**Strict layering rule**: the manager handles only processes and output pipes, with no LLM/session semantics; all semantics (budgets, rate limiting, truncation, injection, communication) live in the extension.

## 3. pi-famulus (Rust)

### 3.1 Singleton and startup

Conventional paths (Unix; Windows uses named pipe `\\.\pipe\pi-famulus`). **The base directory can be overridden with `PI_FAMULUS_HOME`** (essential for tests and multi-instance debugging; the CLI also supports the global flag `--home <dir>`, with priority: flag > env > default):

```
~/.pi/agent/pi-famulus/
├── manager.sock          # unix domain socket
├── manager.pid           # {pid, version, started_at} JSON
├── manager.lock          # daemon lifetime lock (flock, held for the daemon's entire lifetime)
├── manager.spawn.lock    # client spawn lock (valid while fd-lock is held)
├── manager.log           # manager's own log
├── config.json           # optional user configuration
└── sessions/<session_id>/tasks/<task_id>.json    # task state
                                └─ <task_id>.output   # full merged stdout+stderr stream
```

Startup flow (client side, whether extension or CLI):

1. Connect to the socket; success → `hello` handshake, use it if the version is compatible
2. Failure → attempt to acquire `manager.spawn.lock` (fd-lock, nonblocking trylock)
3. Acquired → spawn `pi-famulus daemon` (detached) → poll until the socket is ready (2s timeout) → release the lock
4. Not acquired → someone else is spawning; poll until the socket is ready
5. Socket exists but cannot be connected to (zombie socket) → same as 2–4: spawn a daemon and let it clean up. **Clients never delete socket/pid files** (otherwise they could delete the socket of a daemon another client just spawned)
6. `hello` rejected because the manager is gracefully shutting down (§3.2) → wait for it to exit, then follow 2–4. Exact protocol (implemented by Rust CLI `client::connect`, tests `d8`/`d8b`):
   - Recognition: the `hello` response is `{"ok":false,"error":{"code":"E_INTERNAL","message":"manager is shutting down"}}` (`id` is the hello's id), then the manager closes that connection. Match exactly on `code == "E_INTERNAL"` and `message == "manager is shutting down"`; other `E_INTERNAL` errors do not qualify. This response returns immediately (the manager still accepts during shutdown), without waiting for the hello timeout
   - Waiting: read `pid` from `manager.pid`, probe with `kill(pid, 0)` every 50ms until the process no longer exists or 5s have elapsed (graceful shutdown needs at most the 2s kill grace + final cleanup). If the pid file cannot be read, do not wait. Delete no files and do not reconnect to the old connection while waiting
   - Afterwards: follow 2–4 (acquire spawn lock → spawn → wait 2s for socket), then reconnect + `hello` once; report an error only if this still fails. Continue this way even if the old manager remains after 5s: the newly spawned daemon cannot acquire `manager.lock`, exits with "already running", and this connection is treated as an ordinary failure

Ownership of `manager.pid` and the socket: daemon identity = the exclusive flock on `manager.lock`, held from startup to exit (released by the OS on a crash; the fd is CLOEXEC and not inherited by tasks). At startup the daemon tries the lock: failure → another daemon exists, print "already running" and exit 0; success → socket/pid files must be stale, remove them, bind, and write the pid. Do not use pid liveness as identity (an unrelated process can reuse a pid). `doctor` follows the same rule: clean stale files only after acquiring the lock.

### 3.2 Lifecycle (hard anti-zombie semantics)

- Each extension connection registers `{session_id, pi_pid}` at `hello`; that connection is the session's control channel
- Connection closes (process death necessarily causes this with Unix sockets, including kill -9) → mark that session disconnected
- **Zero active connections for 5s → graceful shutdown**:
  1. Send SIGTERM to all running tasks (process groups); include tasks whose leader exited but whose group still contains members (background children)
  2. 2s grace → send SIGKILL to **process groups** that may still have members (even with a dead leader, SIGTERM-ignoring descendants are killed)
  3. Persist task state as `killed` (reason: "manager_shutdown")
  4. Remove socket/pid files and exit
- During shutdown the manager still accepts new connections but immediately rejects their `hello` (`E_INTERNAL` "manager is shutting down"); clients follow §3.1 step 6, waiting for this manager to exit before spawning a successor rather than hanging until the response timeout
- Background tasks must not outlive the last pi. Reattachment with `pi --resume` works only while "another pi is still alive"
- The manager **never resurrects itself**; only clients (extension/CLI) spawn it when needed
- **The manager is the parent of every task; however it ends, tasks are cleaned up with it (lifeline)**: each task starts with `pi-famulus __run <command>` (runner) as process-group leader; the runner spawns `sh -c <command>` as a child in that same group (`sh.spawn()`; the runner does not exec and remains in the group). At fd 3 the runner holds the lifeline: the read end of a pipe whose write end is held only by the daemon. However the daemon ends (`shutdown`, `kill -9`, panic), the kernel closes the write end; the runner reads EOF → SIGTERM its own process group → 2s → SIGKILL. **The guarantee covers process-group members only**: if a command moves a descendant into a new session (e.g. `setsid`), it leaves this group and the lifeline cannot control it—a known boundary, no isolation provided. **No crash recovery**: a new daemon adopts no tasks at startup (see §3.4).
  - At fd 4 the runner reports the command's real exit code/signal through a status pipe (`exit <code> <alone|linger>` or `signal <n> <alone|linger>`). If the command leaves background children (`cmd &`), the runner remains as a guardian, holding the lifeline until it is the group's only member; the daemon treats "status received" as task completion and "runner exited" as an empty process group.
  - The runner blocks SIGTERM (the child unblocks it before exec), so stop/shutdown's group SIGTERM affects only the command and the runner can report how the command ended; only SIGKILL takes the runner itself down, in which case the daemon falls back to the runner's wait status.
  - The lifeline write end has only one holder (`task::lifeline()`, CLOEXEC), making a later in-place exec handover a single-fd operation for that end.
- **In-place upgrades (exec handover, transparent to running work)**: after binary replacement, the daemon `exec()`s the new binary, **keeping the same pid**; every runner remains its child (`waitpid` works normally, exit codes are real), tasks are uninterrupted, and users need not decide "is it safe to upgrade now".
  - Trigger: `pi-famulus upgrade`; or the daemon checks its own executable every 2s (dev/inode/size/mtime), automatically upgrading once a changed file stays stable for another cycle (`trigger: "binary-changed"`). Still install with `install` (new inode, macOS signature cache).
  - Steps: ① preflight: run `<new binary> __handover-check`, which must return the same handover format version; incompatible/broken binaries stop here without changing anything (no retry until that file is replaced). ② quiesce: task output pumps pause only between reads, exit waits are suspended, fanout drains already-read content, all client connections close and writers flush; limit: 5s quiesce + 2s flush; on timeout abandon the upgrade and resume the old image. ③ Write `<home>/handover.json` (versioned: task records, kill requests/reasons, **absolute** timeout and kill-grace deadlines, guardian state, fd numbers), clear CLOEXEC on inherited fds, exec. ④ Exec failure: restore CLOEXEC on fds, continue tasks, record the result in `status.last_upgrade`, and serve normally from the old image.
  - Fds inherited across exec: listening socket (**no rebind**; new connections wait in the backlog during handover and are not refused), `manager.lock`, both lifeline ends (closing the write end would clean up all tasks), each task's stdout/stderr/status pipe read ends.
  - The new image restores with `daemon --handover <file>`; restore failure means exit, breaking the lifeline and cleaning up every task—the same semantics as a crash (no crash recovery, no rescue attempt).
  - After restore there is a **30s handover grace** (`handover-grace`), suspending the "zero connections for 5s → shutdown" rule while clients reconnect.
  - The measured client-visible disconnection window is 30–46 ms (under parallel test load); in-flight requests during handover **receive no response**, as connections close directly. After reconnecting + hello, clients resend: idempotent requests (wait/output/list/watch/status/stop/mark_background) directly; `start` with the same `key` (see §3.3).
  - A session's pre-handover watches are restored automatically at re-hello, replaying its missed output range `[cursor delivered before handover, current)`, without gaps or duplicates.
  - `status` adds `generation` (in-place upgrades experienced by this pid) and `last_upgrade` (`at, ok, from_version, to_version?, error?, trigger: "cli"|"binary-changed"`).
- **One-time name transition**: switching to this project name, binary name, environment namespace, and home directory is a breaking installation change, not an in-place upgrade. Wait for existing work to finish or stop it, close the sessions using the previous installation, and wait for its daemon to exit before reinstalling and reopening sessions. Migrate configuration only into `~/.pi/agent/pi-famulus/config.json`, updating explicit binary/home paths and environment overrides. Do not move runtime state or history wholesale: task and agent records contain absolute output/transcript paths, and moving their directory does not rewrite those references. Keep previous history separately if needed. Subsequent compatible upgrades under the same name and home retain the in-place handover support above.
- **Disconnected-session retention**: sessions leave `ls` / `sessions` immediately after disconnecting; `sessions/<sid>/` remains for `goneSessionRetention` after its last write (config.json, default 24h), during which `show` / `agent` / `events` still work (postmortem inspection, `pi --resume`). On expiry the daemon deletes the directory and removes its tasks from memory. Sweep at startup and every `min(retention, 1h)`; connected sessions, or sessions still owning running tasks/live process groups, are never swept

### 3.3 Transport and protocol

- Frame: `u32 BE length + UTF-8 JSON payload`, maximum frame 4 MiB
- Extension: one long-lived connection per session (multiplexed requests/responses/events); CLI: short-lived connection, closed after a single request-response
- Request: `{"v":1, "id":"<uuid>", "type":"...", ...}` (**all requests, including hello, carry v+id**; examples below omit v/id for brevity)
- Response: `{"v":1, "id":"<uuid>", "ok":true, ...}` or `{"v":1, "id":"...", "ok":false, "error":{"code":"E_*","message":"..."}}`
- Event (server push, no id): `{"v":1, "type":"event", "event":"...", ...}`

**Version compatibility (N−1, a prerequisite for in-place upgrades)**: after an upgrade, running pi sessions still have the previous extension loaded and immediately reconnect with its protocol. Rules:
- Frame version `v` remains 1; only a mismatch here returns `E_VERSION`; hello's `protocol` level is informational, and older levels are accepted normally.
- All new fields are optional (`#[serde(default)]`), taking defaults when older clients omit them; new requests/events must not change existing request/event semantics.
- Tests enforce: an old-protocol hello still works normally after handover.

**Output cursor contract**: an output event's `next_cursor` and an `output` request's `cursor`/`next_cursor` share the same **monotonically increasing byte offset** (in the merged `.output` stream). An output event covers `[next_cursor − bytes(chunk), next_cursor)`. Chunk boundaries are UTF-8 safe (events hold back incomplete trailing bytes, carrying them across handover). Calling `output` at the end returns an empty chunk and `next_cursor == cursor` (caught up). Clients use these byte ranges to deduplicate/trim their own `output(cursor)` catch-up reads and post-handover replays.

**`start` idempotency**: `start` may carry an optional client-generated `key`; resending the same key within the same session returns the first task started, without starting another; keys survive in-place upgrades.

**Supplemental rulings (2026-09-17)**:
- `kind:"shell"` commands are interpreted by a shell: Unix uses `env.SHELL -c` (`/bin/sh -c` if env omits SHELL); Windows uses `cmd /c`
- Omitting `env` from `start` → the child inherits the manager's own environment (the extension always explicitly passes the complete env)
- `stop` response = signal sent (SIGTERM→2s→SIGKILL flow started); the state change is determined by `task_exited`; killed tasks' `task_exited` carries `exit_code:null, signal:"SIGTERM"|"SIGKILL"`
- CLI output is human-readable tables (not a contract); successful `shutdown` exits 0

#### Message definitions

**hello** — Must be the first message after connecting:
```json
→ {"type":"hello", "client_kind":"extension", "session_id":"<pi session id>", "pi_pid":1234, "cwd":"/path",
   "extension_version":"0.3.0", "protocol":2}
→ {"type":"hello", "client_kind":"cli"}
← {"ok":true, "version":"0.1.0", "pid":4321, "started_at":1726...}
```
- `extension` must carry `session_id` + `pi_pid`; thereafter this connection receives that session's events
- `cwd` is optional (backward compatible). The extension includes the session cwd in hello; the manager stores it in the session and returns it in status/sessions. Older clients omitting it can still handshake
- `extension_version` (string) and `protocol` (integer) are optional: the manager stores them per session and returns them in `status.sessions`; `doctor` uses them to check that each connected session's protocol matches the manager. `protocol` is a feature level: 1 = original §3.3, 2 = observability contract (origin / mark_background / stop.reason / end_reason / events.jsonl), 3 = in-place upgrade (`upgrade`, status's `generation`/`last_upgrade`/`exe`, start key, resend after reconnect). CLI `upgrade` sends no request to a manager below level 3 and directly asks for a one-time restart. Omitted = older extension
- Duplicate hello for the same `session_id`: new connection wins; the old connection receives `{"type":"event","event":"session_rebound"}` and is closed by the server
- `cli` carries no session; it can access cross-session read-only/management operations

**start** — Start a process:
```json
→ {"type":"start", "kind":"shell"|"monitor", "command":"...", "cwd":"...",
   "env":{...}, "run_in_background":false, "timeout_ms":null,
   "origin":{"via":"child-bash", "child_id":"ch_…", "run_id":"run_…"}}
← {"ok":true, "task_id":"sh_a1b2c3d4", "pid":5678}
```
- `session_id` comes from the connection binding
- `env` is the child's **complete environment** (the client constructs it; the extension passes `process.env` + injected `PI_*`)
- `timeout_ms`: hard kill ceiling; `null` = unlimited (default for background tasks)
- `run_in_background:true` is only a semantic marker (the client no longer waits); manager behavior is unchanged
- `origin` is optional, recording the initiator: `via` is `"bash-fg"` (foreground bash, possibly backgrounded later) | `"bash-bg"` | `"child-bash"` (child agent's bash, with `child_id`/`run_id`) | `"monitor"`; the manager stores it unchanged in TaskRecord (new values do not make start fail)
- task_id format: `<kind prefix>_<8 hex digits>`; prefixes `sh` (shell) / `mon` (monitor) / future `ag` (agent)

**wait** — Wait for exit (within a budget):
```json
→ {"type":"wait", "task_id":"sh_a1b2c3d4", "budget_ms":20000}
← {"ok":true, "done":true, "exit_code":0}
← {"ok":true, "done":false}                  // budget expired, task keeps running
```

**output** — Read output incrementally:
```json
→ {"type":"output", "task_id":"sh_a1b2c3d4", "cursor":0, "max_bytes":65536}
← {"ok":true, "chunk":"...utf8-lossy...", "next_cursor":12345, "status":"running",
   "exit_code":null, "total_size":23456}
```
- cursor is a byte offset; use `next_cursor` for the next incremental read
- chunk is a UTF-8 lossy string; v1 does not preserve binary data faithfully
- Chunk boundaries never split UTF-8 characters: cut points back up to character boundaries, introducing no U+FFFD for valid text and skipping no bytes (`next_cursor` points to the first unsent byte). Incomplete trailing sequences are held back while the task is running. If the first character itself exceeds `max_bytes`, send it whole (chunk can exceed the cap by at most 3 bytes); `max_bytes:0` returns an empty chunk
- JSON-escaped chunk size is constrained by the frame limit (control characters escape to 6 bytes); responses never exceed 4 MiB. Any response that cannot fit in one frame becomes an `E_INTERNAL` error, with the connection remaining usable
- `watch`'s `output` events obey the same character-boundary rules

**mark_background** — The extension notifies the manager when moving a task to the background (foreground budget exhausted or run_in_background):
```json
→ {"type":"mark_background", "task_id":"sh_a1b2c3d4"}
← {"ok":true}
```
The manager records `backgrounded_at` (ms) in TaskRecord; only the first value is kept; no-op if the task has ended.

**stop**:
```json
→ {"type":"stop", "task_id":"sh_a1b2c3d4", "reason":"tui"|"cli"|"tool"|"timeout"|"rate-limit"|"session-end"}
← {"ok":true}
```
`reason` is optional (defaults to `"tool"`, compatible with older extensions); unknown values → `E_BAD_REQUEST`. CLI `stop` sends `"cli"`.
SIGTERM process group → 2s → SIGKILL (sent to the group even if its leader exited). Terminal state `killed`. For an ended task with leftover background children, `stop` also cleans up its group without changing its status.

**end_reason** (TaskRecord and `task_exited` event): why a task ended, one of
`exited` (natural exit, any code) | `timeout` | `stopped:tui` | `stopped:cli` | `stopped:tool` | `rate-limit` | `session-end` | `manager-shutdown` | `manager-crash`.
Mapping: stop with reason X → `stopped:X`, except timeout / rate-limit / session-end map to themselves; `timeout_ms` expires → `timeout`; `shutdown_session` → `session-end`; graceful manager shutdown → `manager-shutdown`; manager dies without shutdown (kill -9, panic), next daemon startup marks still-running records `orphaned` → `manager-crash`. First reason wins (shutdown after stop does not overwrite `stopped:cli`).

The `signal` field (`task_exited` event and TaskRecord) is a signal-name string, e.g. `"SIGTERM"` / `"SIGKILL"`; `null` on normal exit. Numbers written by older versions still load (converted to names).

**list**:
```json
→ {"type":"list", "all":false}
← {"ok":true, "tasks":[{...TaskRecord}]}
→ {"type":"list", "all":true, "paged":true, "after":"<next>"}   // pagination
← {"ok":true, "tasks":[...], "next":"<started_at>/<task_id>"}  // no next = last page
```
- Extension connection: own session only; CLI connection: `all:true` or an explicit `session_id` can inspect all
- Sorted by `(started_at, task_id)`. Without `paged`, return the whole table in one frame, or `E_INTERNAL` if it exceeds 4 MiB; with `paged:true`, fill each page to the frame budget (at least one record), include `next` if more remain, and use it as `after` for the next page. The cursor is the last record's `started_at/task_id`, so deletion between pages does not shift subsequent records. CLI `sessions` / `ls` / `show` / `kill-session` all fetch pages; older daemons unaware of `paged` ignore it and return no `next`, so the CLI still fetches only once

**watch / unwatch** — Subscribe to a task's output-stream events:
```json
→ {"type":"watch", "task_id":"mon_x"}
← {"ok":true}
// subsequent server pushes:
← {"type":"event", "event":"output", "task_id":"mon_x", "chunk":"...", "next_cursor":100}
```

**shutdown_session** — Stop all tasks in this session:
```json
→ {"type":"shutdown_session"}
← {"ok":true, "stopped":["sh_a","mon_b"]}
```

**upgrade** (CLI only, protocol ≥3):
```json
→ {"type":"upgrade"}
← {"ok":true, "from_version":"0.1.0+066598ae00", "generation":0}
```
Ask the daemon to upgrade in place to the current file at its own path (quiesce → exec → restore, same pid). The response confirms only "started", not success. Read the actual outcome from `status.generation` (+1 per successful upgrade) and `last_upgrade` (below). The upgrade quiesces and closes the current connection; the CLI reconnects to the same pid following §3.2, and `start`'s `key` makes resent requests idempotent. Errors: non-CLI connection → `E_FORBIDDEN`; manager shutting down or another upgrade underway → `E_INTERNAL`. Managers below protocol 3 have no such message; the CLI checks `status.protocol` first and, below 3, sends no request and directly asks for a one-time restart.

**status** (read-only, both CLI and extension):
```json
→ {"type":"status"}
← {"ok":true, "version":"0.1.0+066598ae00", "pid":4321, "uptime_ms":3600000, "protocol":3,
   "generation":1, "exe":"/usr/local/bin/pi-famulus",
   "last_upgrade":{"at":1726...,"ok":true,"from_version":"0.1.0+abc1234500",
                    "to_version":"0.1.0+066598ae00","trigger":"cli"},
   "sessions":[{"session_id":"...","pi_pid":1234,"connected":true,"cwd":"/path",
                "extension_version":"0.3.0","protocol":2,"connected_at":1726...,"last_seen":1726...}],
   "task_counts":{"running":2,"terminal":5}}
```
`protocol` is the manager's protocol level; `connected_at` = this manager's first hello from that session (unchanged on reconnect); `last_seen` = most recent request or disconnect (current time while connected). `generation` is this pid's count of in-place upgrades, starting at 0. `last_upgrade` is the latest upgrade attempt (success or failure), omitted if none; `error` (only on failed upgrades) explains why the switch did not happen and the daemon still runs the old binary; `trigger` is `"cli"` (`pi-famulus upgrade`) or `"binary-changed"` (the daemon noticed its file was replaced). `exe` is the daemon's own binary path: an upgrade `exec()`s the current file there, which is not necessarily the requesting CLI's own path (`pi-famulus upgrade` notes when they differ).

**shutdown** (CLI): trigger the same graceful shutdown as "zero connections".

#### Events

| event | Fields | Push condition |
|---|---|---|
| `task_started` | task_id, kind, command, pid, ts | Always (to owning session) |
| `output` | task_id, chunk, next_cursor | Only after watch |
| `task_exited` | task_id, exit_code, signal, duration_ms, output_path, output_size, ts, end_reason | Always (except orphan marking during crash scan, below) |
| `session_rebound` | — | To the replaced old connection |

Note: `task_exited`'s "always" means while the daemon lives. When the crash scan (§3.4) marks still-running records orphaned, it **pushes no events**—the old daemon is dead, with no session to push to. Clients learn the change through `list` after reconnect (the extension's reconnect reconcile) or `wait`; do not wait for an event that will never arrive.

#### Event log (events.jsonl)

- File: `<home>/sessions/<session_id>/events.jsonl`, append-only, one JSON object per line; sessionless daemon events go to `<home>/events.jsonl`
- Each line < 4 KiB (including newline): overlong string fields are truncated (ending in `…`) with `"truncated":true`; `src`/`type`/`ts` are never truncated. Each line is written with one O_APPEND `write`, so concurrent manager/extension appends cannot interleave
- Common fields: `ts` (ms), `src` (`"manager"` | `"extension"`), `type`, `id?` (task/child id), plus type-specific fields
- Manager writes: `session.connect {pi_pid, cwd, extension_version, protocol}`, `session.disconnect {reason: closed|rebound}`, `task.start {kind, command(≤200 characters), origin, pid}`, `task.background {after_ms}`, `task.stop {reason}`, `task.exit {exit_code, signal, end_reason, duration_ms}` (including orphaned and force-ended tasks at shutdown), `daemon.start {pid, version, protocol, orphaned, loaded}` / `daemon.shutdown {pid, killed_tasks}` (also in manager.log)
- Extension writes: `wake.emit {kind, ids[], batch}`, `wake.deliver {kind, mode: trigger|steer|passive}` (passive = `triggerTurn:false`, no new turn: clean monitor exit within 2s of an event, leftover lines and exit from a monitor stopped by the model), `wake.dedupe {id}`, `monitor.drop {id, lines}`, `monitor.stop {id, reason}`, `agent.start {child_id, run_id, name, agent, model}`, `agent.settle {child_id, status, error?, stalls?, duration_ms}`, `agent.stall {child_id, attempt}` (written on **every stall detection**, including before auto-resume; does not mean failure—use `agent.settle` with error=stalled to determine failure), `agent.timeout {child_id}`, `decision.request/reply/timeout {child_id}`
- Readers (CLI `events`/`show`/`sessions`) skip unparseable lines or lines missing `ts`/`type`
- Retention: stored with the session directory; no rotation in v1 (deferred: rotation | impact: disk growth for very long sessions | trigger: doctor reports sessions directory > 100MB)

#### Agent records (extension-owned, `<home>/sessions/<sid>/agents/`)

- `<ch>.json`: `{child_id, run_id, session_id, name, agent, model?, status, started_at, ended_at?, error?, attempts?(total generation count, written only if >1), end_reason?(completed|failed|model-error|stalled|timeout|interrupted|disposed), prompt_head(task prompt only, no agent preamble, ≤2000), result_tail(≤2000), tool_calls, transcript}`
- `<ch>.jsonl` transcript: one message per line `{role, text, tool?, args?, isError?, ts}`, appended live
- The CLI only reads these files (`ls`/`show`/`agent`/`log`); if a record says running but its owning session is disconnected, the CLI treats it as `interrupted` and `doctor` reports a stale record

#### Error codes

`E_NOT_FOUND` / `E_BAD_REQUEST` / `E_VERSION` / `E_SESSION_REQUIRED` / `E_FORBIDDEN` (CLI exceeds its authority) / `E_INTERNAL`

### 3.4 Task model and state machine

```
running ──exit 0──► completed
       ──exit≠0──► failed
       ──stop────► killed
       ──manager crash (marked at next startup)──► orphaned
```

- TaskRecord (on disk `<task_id>.json`): `{task_id, session_id, kind, command, cwd, pid, status, exit_code, signal, started_at, ended_at, output_path, output_size, origin?, backgrounded_at?, end_reason?}` (last three added by the observability contract, optional; older records still load)
- Output: in-memory ring buffer (64KB) + full append-only disk log; `output_size` increases monotonically
- **Startup after a crash (no crash recovery)**: read the state directory. Still-running records belong to a daemon that died without shutdown; its runners have already seen the lifeline break and cleaned up their process groups (§3.2), leaving nothing to adopt: mark records `orphaned` (`end_reason:"manager-crash"`, `ended_at` set to startup time), persist them, and recover `output_size` from file length. **Send no signal to pid/pgid from old records** (pids may have been reused).

### 3.5 CLI (inspection and management, user-facing)

Single binary, `pi-famulus <subcommand>`; everything except `daemon` is a client. Full manual: `docs/cli.md`.

```
pi-famulus daemon [--foreground]        # run in foreground (used when spawned)
pi-famulus status [--json]              # version/protocol/uptime/sessions/task and agent counts; never starts daemon
pi-famulus sessions [--json]            # connected sessions (also disconnected ones with running tasks)
pi-famulus ls [--session P] [--cwd D] [--since DUR] [--json]   # all work of connected sessions + anything still running
pi-famulus show <id> [--json]            # any sh_/mon_/ch_/run_ id, fuzzy matching
pi-famulus agent <ch_id> [--full] [-f]   # render agent transcript
pi-famulus events [-f] [--session P] [--id X] [--since DUR] [--json]
pi-famulus log|tail [ID]                # manager.log / task output / agent transcript
pi-famulus output|wait|stop <id>         # stop errors on agents (children run inside pi)
pi-famulus kill-session <session_id>
pi-famulus doctor                       # health checks; any FAIL → exit 1
pi-famulus shutdown
```

### 3.6 Rust structure

```
manager/
├── Cargo.toml
└── src/
    ├── main.rs       # entry point and subcommand dispatch
    ├── cli.rs        # usage-rs CLI declarations
    ├── daemon.rs     # listener, accept loop, connection registration, zero-connection shutdown
    ├── proto.rs      # frame codec + serde message types
    ├── task.rs       # spawn (via runner, process groups), lifeline, output tee
    ├── runner.rs     # `__run`: task process-group leader, lifeline, status reporting, leftover-child guardian
    ├── sys.rs        # sole production unsafe seam: setsid(pre_exec) + kill(pgid)/liveness
    ├── registry.rs   # task registry + session namespaces + disk persistence
    ├── lifecycle.rs  # spawn lock, pid claim, startup scan after crash (mark orphaned)
    ├── client.rs     # CLI: connect/spawn flow, daemon commands, log/tail, doctor
    ├── inspect.rs    # CLI: status/sessions/ls/show/agent/events and data layer
    ├── events.rs     # events.jsonl writes (manager) and reads (CLI)
    ├── fmt.rs        # display width (CJK), durations, local time
    └── out.rs        # stdout that exits quietly with 0 when a pipe closes
```

Dependencies: tokio (full), usage-rs (derive CLI parser), serde + serde_json, interprocess (cross-platform sockets), fd-lock (spawn lock), libc (process groups wrapped only through `sys.rs`). Windows process groups use Job Objects (v1 may initially use `taskkill /T`).

**Memory bounds (§3.4 hardening)**:
- In-memory ring has a hard 64KB cap (`RING_CAPACITY`); full output goes only to `.output` on disk
- tee→fanout uses bounded mpsc (`CHUNK_CHANNEL_CAP=64`, approximately ≤512KB in flight), preventing slow watches from exhausting the process
- Connection map / sessions grow and shrink with connections; terminal task entries remain for list/output (not unbounded growth in the running tee)
- Zero active connections → kill tasks → remove socket/pid → exit (§3.2)

## 4. pi extension (TypeScript)

Directory: `extension/`; `package.json` declares `"pi": {"extensions": ["./src/index.ts"]}`.

### 4.1 manager client (`src/manager-client.ts`)

- `connect()`: §3.1 startup flow → hello(session_id=pi session id, pi_pid=process.pid)
- Multiplex requests/responses (id → Promise map); register event callbacks (`onEvent`)
- Disconnection: reconnect with exponential backoff (0.5s/1s/2s, at most 3 attempts), then re-hello; **on complete failure bash falls back to pi's built-in local execution** (degrade, do not hang)
- `session_shutdown` → `shutdown_session` → close connection

### 4.2 bash override (`src/bash-override.ts`)

Use `createBashToolDefinition(cwd)` for schema/renderer, implement execute ourselves:

```
schema: { command: string, timeout?: number(seconds, hard kill ceiling), run_in_background?: boolean }

execute:
  manager.start({kind:"shell", command, cwd, env: process.env + injected PI_*, timeout_ms})
  run_in_background → immediately return background notice
  otherwise wait(foregroundBudgetMs, default 20000, configurable):
    done → read full output → truncate tail (2000 lines / 50KB, same as built-in) → {content, details:{truncation, fullOutputPath}}
    budget expired → return one-line background status: `⏵ sh_x running in background · /tasks`
      (no instructions or duplicate output path in transcript)
      details: {backgrounded:true, task_id, fullOutputPath}; no-poll/end-turn guidance stays model-facing in tool guidelines
```

- `details` remains `BashToolDetails`-compatible (truncation/fullOutputPath); extension fields are added to details
- **Bare-sleep interception**: patterns such as `^\s*(sleep\s+\d|while true|until ...)` → error, suggest `monitor` or `run_in_background`
- **Bash inside child agents** (M3): register a variant forbidding auto-backgrounding (budget=0 semantics, timeout kills directly and reports an error)—lain's waitUntilExit lesson

### 4.3 task_* tools (`src/task-tools.ts`)

- `task_list({all?})` → manager list **merged** with in-process subagent children (extension registry + `sessions/<sid>/agents/*.json`); `pi-famulus ls` reads those persisted records too
- `task_output({task_id, cursor?, max_bytes?})` → manager output; return tail + file pointer
- `task_stop({task_id})` → manager stop

### 4.4 monitor tool (`src/monitor.ts`)

```
monitor({ command, description, timeout_ms = 300000 (min 1000, max 3600000),
          persistent = false })
```

- `manager.start({kind:"monitor", run_in_background:true})` + `watch(task_id)`
- Extension-side line handling (pure functions, easy to test):
  - `LineBatcher`: chunk → split on `\n` → 200ms batching; per-line cap 500 characters, per-batch cap 3000 characters
  - `RateLimiter`: token bucket (capacity 10, +1 every 2s); at least 10 batches in the last 30s with ≥50% dropped → automatic stop + notification
- Event injection: `<pi-famulus-wake kind="monitor">` (see §4.5); idle→triggerTurn, busy→steer; while busy, coalesce per monitor and send after `agent_settled`, carrying `event-count` and `dropped-lines`
- Process exit → end notification; timeout expires → stop + "[Monitor timed out — re-arm if needed.]"
- `persistent:true` → lives until session ends (no timeout)
- All time sources and timers use the `Clock` injected from extension scope, including batching, rate limiting, and timeout; tests use `ManualClock`, not global fake-timer replacement
- Prompt wording (misuse prevention): commands must be line-buffered; "silence is not success" (grep must cover failure patterns); events are not user replies; do not poll

### 4.5 NotifyCenter (`src/notify.ts`)

The sole injection outlet for all asynchronous events:

```ts
notify({ customType, content, details }): void
// idle → pi.sendMessage(msg, {triggerTurn:true})
// busy → pi.sendMessage(msg, {deliverAs:"steer"})
```

- 200ms batching window: merge multiple task_exited events into **one** `<pi-famulus-wake kind="task">`, containing multiple `<task>` elements
- Deduplication: send the same event for the same task only once
- All asynchronous injections share one `customType`: `pi-famulus-wake`. The former per-kind notification types are no longer emitted or registered. Their renderers were removed; older transcripts use pi's default custom-message rendering.
- The lead-in is just one sentence, exported as `FAMULUS_WAKE_LEAD_IN`:

```ts
export const FAMULUS_WAKE_LEAD_IN =
  "System wake — not a new user message. Handle this <pi-famulus-wake> before other work.";
```

  content = lead-in + blank line + one `<pi-famulus-wake>`. Replacing the lead-in with `""` still leaves a parseable envelope. The renderer reads only `details.kind`, not a type guessed from XML. Pill color/glyph comes from status/exitCode in details, not summary text.

### pi-famulus-wake contract (authoritative for the eval wake adapter)

Attributes use kebab-case, values are XML-escaped. All child-element text is escaped (including monitor `<event>`). `details` uses camelCase, discriminated by `kind`. `still-running` is a child element, not an attribute; omit if empty. Item text is the display title (shell: command collapsed to one line and capped at 80; agent: name).

| kind | Root attributes | Child elements | details |
|---|---|---|---|
| `task` | (none; still-running is not an attribute) | Optional `<still-running><item id>`; one or more `<task id kind status duration-ms exit-code? signal?>`, containing `summary` `command` `output-file` `preview` | `{ kind:"task"; stillRunning: {id,title}[]; tasks: [{ id, taskKind, status, summary, command, outputPath, preview, durationMs, exitCode: number\|null, signal?: string }] }` |
| `monitor` | `id` `description` `status?` | `<event>` | `{ kind:"monitor"; id; description; status?; event }` |
| `subagent-handover` | `run-id` `child-id` `name` `status` | Optional `<still-running>`; `summary` `prompt` `result`; optional `error` | `{ kind:"subagent-handover"; runId; childId; name; status; stillRunning: {id,title}[]; summary; prompt; result; error? }` |
| `subagent-done` | `run-id` `status` `duration-ms` | `summary`, then one `<child id name status>` per child, containing `prompt` (head capped at 2000), optional `error`, `result` (tail capped at 2000) | `{ kind:"subagent-done"; runId; status; durationMs; summary; children: [{ childId, name, status, prompt, result, error? }] }` |
| `supervisor-request` | `from` `name` | `message`, `reply-with` | `{ kind:"supervisor-request"; from; name; message }` |
| `supervisor-update` | `from` `name` | `message` | `{ kind:"supervisor-update"; from; name; message }` |

- Omit the `exit-code` attribute when `exitCode === null`; details always uses `number | null`. `signal` is a signal-name string (`"SIGTERM"` | `"SIGKILL"`), omitted if absent, not a number.
- `reply-with` text is `agent_message { action: "reply", to: "<childId>", message: "<your decision>" }`, not included in `message`.
- The subagent-done pill shows per-status counts, e.g. `3 completed · 1 failed`.
- Behavior guidelines are a persistent section, not the entire systemPrompt returned by `before_agent_start`. Handle `<pi-famulus-wake>` before continuing; do not poll/sleep/fabricate results.

### 4.6 subagent tool (M3)

Modules: `src/subagent/` (types.ts / runner.ts / registry.ts / pool.ts / tool.ts / child-bash.ts / pi-runtime.ts / fleet-widget.ts)

**Strict pi-runtime isolation rule**: `createAgentSession` may be **dynamically imported** only in `pi-runtime.ts` (`await import("@earendil-works/pi-coding-agent")`); all other modules have zero runtime pi dependencies (`import type` is allowed). The runner creates child sessions through an injected `CreateSessionFn` factory, faked in tests. If dynamic import fails, the subagent tool returns explicit error text without affecting other tools.

**Child-session construction** (pi SDK verified): `createAgentSession({ cwd, model?, thinkingLevel?, tools: allowlist, customTools: [contact_supervisor, etc.], sessionManager: SessionManager.inMemory() })`; model resolution uses `ctx.modelRegistry.find(provider, id)` / `getAvailable()`; default model = parent session's current model (`ctx.model`). Child-session objects remain in the registry until session_shutdown, so "resume" = another `prompt()` on the same object (v1 does not reconstruct across processes).

**Tool schema** (typebox, field-name contract):

```
subagent({
  tasks?: [{ agent?: string, prompt: string, name?: string }],   // parallel, 1..10
  chain?: [{ agent?: string, prompt: string, label?: string }],  // sequential, forced synchronous execution
  async?: boolean,            // true=return run_id immediately
  concurrency?: number,       // 1..8, default 4
  fail_fast?: boolean,        // default false; stop only unstarted tasks, let started tasks finish
  model?: string,             // fuzzy model spec, see "Model resolution" below; optional ":<thinking>" suffix
  timeout_ms?: number,        // hard timeout per child, default 1800000, maximum 3600000
  action?: "list"|"get"|"status"|"interrupt"|"resume"|"steer"|"models",   // manage existing runs
  run_id?: string,            // action target
  child_id?: string,          // steer/interrupt/resume may target a single child
  message?: string,           // steer/resume content
})
```

- `tasks` and `chain` are mutually exclusive, and both exclude `action`; exactly one of the three is required
- Synchronous path: wait until all finish or **subagentBudgetMs (default 45000, configurable)** expires → switch to asynchronous execution, immediately return `{ run_id, status: "backgrounded" }` + "You will be notified on completion; do not poll" wording; on completion NotifyCenter injects `<pi-famulus-wake kind="subagent-done">`
- Chain interpolation: `{previous}` = previous node's result text; `{outputs.<label>}` = result from the named node; undefined label reference → immediate error, start nothing
- Parallel: worker pool (concurrency slots), results returned in tasks-array ordinal order
- One child failure does not bring down the group: mark that result entry `status:"failed", error`; fail_fast=true cancels unstarted entries
- Result text: each child's `session.getLastAssistantText()`; empty → "(no output)"
- Depth: extension records its own depth (main=0); child-session tools **exclude subagent** (hard depth-1 cap, no deeper nesting in v1)
- **Child session isolation**: `createPiSessionFn` explicitly passes `DefaultResourceLoader({ cwd, agentDir, noExtensions:true, noSkills:true, noPromptTemplates:true, noThemes:true, noContextFiles:true })`; no user/project extensions, skills, prompt templates, themes, or context files load. In particular, never load the parent extension, whose session_start / before_agent_start would inject parent wake guidelines into the child prompt. Children still receive `CHILD_BEHAVIOR_GUIDELINES` separately; no configuration switch.
- **Unified clock and generation timer ownership**: the extension creates one `Clock` and injects it into time-dependent services. `ManualClock` deterministically runs timers by deadline, then insertion order for ties; already-due timers created during `advance()` also run, clearing a timer inside a callback prevents later execution, and intervals crossed by a large advance fire once per due point. Each child generation owns a `TimerScope`; settle, interrupt, resume, or dispose clears all timeouts in that scope, preventing expired generations from affecting later state. No Effect-TS required; the rejected pilot's rationale and measurements are in `docs/decisions/effect-child-runner-pilot.md`.
- Child bash: `child-bash.ts` forbids backgrounding—schema has no `run_in_background`; execute uses manager start + wait (full timeout_ms), SIGKILLs on expiry and returns a timeout error (never backgrounds); bare-sleep interception matches main bash
- Limits: global concurrency 8 (across runs); stall watchdog—no child events for `stallMs` (default 5min) → abort and automatically continue on the **same session** (continuation prompt, transcript preserved), up to `stallRetries` (default 1), only then mark `failed (stalled)`; session spawn budget 32 children/hour, error on excess
- Management actions: `list` (all runs + state in this session), `get` (run_id → full results), `status` (run_id → each child's state/elapsed time/last event), `interrupt` (abort a child or entire run), `steer` (running child → `session.steer(message)`), `resume` (ended child → continue with `session.prompt(message)`, notify again on completion), **`models` (list selectable models for a pre-call check)**

**Model resolution** (pi multi-provider verified: `modelRegistry.getAvailable()` / `find(provider,id)`; whitelist = settings `enabledModels` / `--models` → `ctx.scopedModels`):

- Candidate set: nonempty `ctx.scopedModels` → scoped only (respect user whitelist); otherwise `modelRegistry.getAvailable()`
- Matching algorithm (pure function `resolveModelSpec(spec, candidates)`): ① exact (`provider/id` or a unique bare-id match) → ② case-insensitive id/display-name substring; 0 matches → error listing candidates; multiple matches → error listing matches, suggesting a `provider/` prefix to disambiguate
- Specs can carry a `:<thinking>` suffix (e.g. `claude-haiku-4-5:high`), overriding the agent definition's thinking after parsing. Valid levels mirror pi core's `VALID_THINKING_LEVELS` (`off, minimal, low, medium, high, xhigh, max`; the SDK does not export the list, so `model-spec.ts` maintains a parity-pinned copy)
- Unknown `:<suffix>` handling mirrors pi's `parseModelPattern(allowInvalidThinkingLevelFallback)`: when the full spec matches nothing, the resolver retries without the trailing suffix; a resolvable base adapts with a **warning** on the child result (never silent), while a still-unresolvable base fails hard with the invalid suffix named in the error. Literal ids containing a colon (OpenRouter `:exacto`) always win — the full spec matches them first, so the retry never fires
- Tool parameter `model` resolution failure → **hard error** (list candidates, LLM can retry); agent-definition file `model` resolution failure → **fall back to parent model** + warning in result details (user-authored files can become invalid across machines; do not fail hard)
- Unspecified: child inherits parent's current model (`ctx.model`)
- `action:"models"` returns one candidate per line, `provider/id — display name`, marking the parent model `(current)` and whitelist source `(scoped)`

**Notification format** (NotifyCenter batching follows task_exited rules):

Completion uses `<pi-famulus-wake kind="subagent-done">`, handover uses `<pi-famulus-wake kind="subagent-handover">`. Shapes are in §4.5; do not use `<subagent-notification>`.

**Fleet widget** (part of M5 brought forward to M3 because it depends on the registry): `ui.setWidget("pi-famulus-fleet", lines, {placement:"belowEditor"})`, only when `ctx.hasUI`; content = one line per active child `● name (agent) — 12s`, cleared with `undefined` when none are active; updates on every registry state transition + a 5s elapsed-time refresh (while active).

### 4.7 comms (M4)

Modules: `src/comms/` (mailbox.ts / tools.ts / routing.ts). **Depend only on the `CommsHost` interface in Appendix B**, never import the subagent implementation (tests use a mock host).

- `contact_supervisor` (registered in child sessions, customTools): `{ reason: "need_decision"|"progress_update", message: string }`
  - `progress_update`: send and return immediately (notify parent through NotifyCenter's `<pi-famulus-wake kind="supervisor-update">`, nonblocking)
  - `need_decision`: block child tool execute until the parent replies; **independent per-child waiter** (no global lock—pi-intercom lesson); 10min timeout returns `"Supervisor did not respond within 10 minutes; decide yourself and continue."`; parent receives `<pi-famulus-wake kind="supervisor-request">` (see §4.5) and answers with `agent_message reply`
- `agent_message` (parent session; child variant carries `from`): `{ action: "send"|"reply"|"broadcast"|"list", to?: string(child_id|name), message?: string, delivery?: "steer"|"queue"(default steer) }`
  - Send to running child: steer → `session.steer(message)`; queue → `session.followUp(message)`
  - Send to ended child: return an error; it does not resume the child. Continue with `subagent({ action: "resume", run_id, child_id, message })`, which notifies again on completion
  - Reply: resolve the child's pending need_decision waiter; no pending request → error listing waiting children
  - Broadcast: steer all active children in the same run
  - List: all children in this session + status + pending requests
- Sibling messages: child-session agent_message carries `from: childId`; routing verifies that source and target share the **same run_id** (lineage check), rejecting cross-run messages with an error
- All communication enters a mailbox log (in-memory ring, 200 entries/run); `list` shows the latest 20

### 4.8 Agent definitions (M5)

Modules: `src/agents/` (definition.ts / loader.ts / builtins.ts). **Pure modules, zero pi dependencies**; index.ts wiring happens during integration.

Markdown files, frontmatter (hand-parsed YAML subset, no dependency):

```markdown
---
name: explorer
description: Fast codebase exploration — finds files, symbols, answers structure questions
tools: [read, bash, grep, find, ls]     # or `read, bash, grep, find, ls`; default = [read, bash, edit, write]
model: anthropic:claude-haiku-4-5       # optional; "provider:id" or bare id
thinking: high                          # optional: minimal|low|medium|high|xhigh
---

You are an explorer agent. ... (body = appended system prompt segment)
```

- Three override tiers (later wins): builtin → `~/.pi/agent/agents/**/*.md` → `<cwd>/.pi/agents/**/*.md`; same-name definitions override, `description` required, `name` must match `^[a-z][a-z0-9-]*$`
- At least two builtins: `explorer` (read-only tools), `worker` (all tools)
- Loading: session_start + lazy reload before each subagent call (mtime cache; skip files that fail parsing and report them in status)
- Unknown name: `subagent({agent:"xxx"})` errors, listing available agent names

### 4.9 Configuration

`~/.pi/agent/pi-famulus/config.json` (read by extension):

```json
{ "foregroundBudgetMs": 20000, "subagentBudgetMs": 45000, "managerPath": null, "logLevel": "info", "goneSessionRetention": "24h" }
```

## 5. Testing strategy

### Rust (`manager/tests/`, black-box integration)

No dependency on crate-internal APIs: spawn the compiled binary and send frames directly with std UnixStream/TcpStream:

- Protocol: round-trips for every hello/start/wait/output/stop/list/watch message
- Events: task_exited push, output push after watch
- Lifecycle: zero connections → manager exits and tasks are cleaned up; spawn-lock singleton (second daemon refuses to start)
- Crash: kill -9 / panic manager → all task process groups are cleaned up with it (SIGTERM; SIGTERM-ignoring members receive SIGKILL only after the 2s grace; stubborn-group cleanup takes about 2s + process-kill time) → after restart, still-running records become orphaned (manager-crash), no adoption, no signals
- Output: incremental cursor reads of large output, UTF-8 lossy decoding

### TS (`extension/tests/`)

- Pure functions: LineBatcher (batching/caps), RateLimiter, notification formatting, result truncation
- Bash override: mock manager client (in-memory protocol implementation) checks budget-triggered backgrounding and fallback paths
- NotifyCenter: batching/deduplication/idle-busy routing

### End-to-end

Manual after M1: run long commands with `pi -e ./extension`, verify automatic backgrounding + notifications + `pi-famulus list`.

## 6. Non-goals (explicitly excluded in v1)

- Cross-session / cross-machine communication (belongs to agent-intercom)
- Detached subagent runner (ChildRunner seam reserved)
- Worktree isolation, workflow-script sandbox, watchdog, missions
- Compatibility with pi-subagents / pi-intercom
- Full Windows support (code paths reserved, unverified)

## Appendix A: TS pure-function signature contract (shared basis for implementation and tests)

**Ruling notes (2026-09-17, added after contract-test disagreements; revised the same day after implementation-side convergence)**:

- `maxLineChars` / `maxBatchChars` are **hard caps, plain cuts, no ellipsis marker**. (The initial ruling counted a marker within the cap; implementation and tests independently converged on plain cuts because the semantics are simplest and caps strict. Monitor-event context makes truncation evident.)
- LineBatcher's timed flush **drains the entire buffer, including an unterminated trailing line** ("no data lost at window end"). (The initial ruling sent only complete lines; implementation converged on drain-at-end, friendlier to progress bars / slow lines—partial lines become visible immediately rather than being held indefinitely.)
- `truncateTail`'s maxBytes is a **hard cap**: if the last retained line alone exceeds it, truncate that line's tail by bytes at a UTF-8-safe character boundary; result always ≤ maxBytes, no U+FFFD overflow
- `truncateTail`'s `totalLines`: original text's line count; empty string = 0 lines. On truncation the first output line is the marker `… (truncated: showing last K of N lines)`
- `formatTaskNotification`: see §4.5. Merge multiple events into one `<pi-famulus-wake kind="task">`; command/preview must be XML-escaped (`& < >`); `exitCode:null` omits `exit-code`
- `formatBackgroundNotice` is one-line task status (`⏵ sh_x running in background · /tasks`); no-poll/end-turn guidance goes only into model-facing tool guidelines
- **Daemon spawn must explicitly pass `--home <resolvedHome>`** (`pi-famulus --home X daemon`); never rely on inheriting PI_FAMULUS_HOME—the caller's home may come from an explicit override rather than the environment (actual bug found in end-to-end integration on 2026-09-17)
- CLI `status` prints aggregate counts (version/pid/uptime/session count/task count); use `sessions` for session details
- Monitor `timeout_ms` is enforced **by the extension** (send manager `timeout_ms:null`): a manager hard kill would degrade timeout notification into ordinary task_exited, unable to produce "[Monitor timed out — re-arm if needed.]"; if the extension dies, the manager's zero-connection cleanup is the fallback
- For foreground-completed commands the extension fetches only a **512KB tail window** from the manager (not all output); `details.truncation.totalBytes` uses the manager's `total_size`

```ts
// extension/src/monitor-batching.ts
export interface LineBatcherOptions {
  flushMs?: number;          // batching window, default 200
  maxLineChars?: number;     // per-line hard cap, default 500 (plain cut, no marker)
  maxBatchChars?: number;    // per-batch hard cap, default 3000 (plain cut, no marker)
  onFlush: (text: string) => void;
}
export class LineBatcher {
  constructor(opts: LineBatcherOptions);
  push(chunk: string): void;   // feed raw output chunk (possibly no newline / multiple lines)
  flush(): void;               // emit current buffer immediately (including a partial trailing line)
  dispose(): void;             // flush + clear timer
}

export interface RateLimiterOptions {
  capacity?: number;          // default 10
  refillIntervalMs?: number;  // default 2000
  refillAmount?: number;      // default 1
}
export class RateLimiter {
  constructor(opts?: RateLimiterOptions);
  tryConsume(n?: number): boolean;  // default n=1; false if insufficient
  dispose(): void;
}

// extension/src/format.ts
export interface TruncationInfo { truncated: boolean; totalLines: number; totalBytes: number }
export function truncateTail(
  text: string, maxLines?: number /*default 2000*/, maxBytes?: number /*default 51200*/
): { text: string } & TruncationInfo;

export interface TaskExitInfo {
  taskId: string; kind: string; command: string;
  status: "completed" | "failed" | "killed" | "orphaned";
  exitCode: number | null; durationMs: number;
  outputPath: string; preview: string;  // caller first truncates preview to 4000
}
export function formatTaskNotification(events: TaskExitInfo[], stillRunning?: WakeItem[], leadIn?: string): FormattedWake;  // §4.5
export function formatBackgroundNotice(taskId: string, command: string, outputPath: string): string;
export function formatMonitorEvent(description: string, taskId: string, batchText: string, status?: string): FormattedWake;  // §4.5
```

## Appendix B: M3-M5 interface signature contract (shared basis for subagent / comms / agents)

```ts
// ---------- extension/src/agents/ (M5, pure modules, zero pi dependencies) ----------
export interface AgentDefinition {
  name: string;                 // ^[a-z][a-z0-9-]*$
  description: string;          // required, nonempty
  tools: string[];              // default ["read","bash","edit","write"]
  model?: string;               // "provider:id" | bare id
  thinking?: "minimal"|"low"|"medium"|"high"|"xhigh";
  systemPrompt: string;         // frontmatter body, trimmed
  source: "builtin"|"user"|"project";
  path?: string;                // no path for builtin
}
export interface LoadReport { definitions: AgentDefinition[]; errors: { path: string; error: string }[] }
export function parseAgentMarkdown(content: string, source: AgentDefinition["source"], path?: string):
  AgentDefinition;            // parse failure throws Error (with reason)
export function loadAgentDefinitions(opts: { userDir: string; projectDir: string }): LoadReport;
                              // three-tier merge: builtin < userDir(**/*.md) < projectDir(**/*.md), later same-name wins
export function resolveAgent(defs: AgentDefinition[], name: string | undefined): AgentDefinition;
                              // undefined → default "worker"; unknown name → throw (message lists available names)

// ---------- extension/src/subagent/model-spec.ts (pure function, zero pi dependencies) ----------
export interface ModelCandidate { provider: string; id: string; name?: string }
export type ModelResolution =
  | { ok: true; provider: string; id: string; thinking?: string; warning?: string }
  | { ok: false; error: "no-match"|"ambiguous"; candidates: string[]; thinking?: string; suffixHint?: string };
export function resolveModelSpec(spec: string, candidates: ModelCandidate[]): ModelResolution;
  // ① Strip ":<thinking>" suffix (VALID_THINKING_LEVELS = off|minimal|low|medium|high|xhigh|max, pi core parity)
  // ② Exact "provider/id" → unique exact bare id → case-insensitive id/name substring
  // ③ Full-spec failure with an unrecognized suffix: strip it and retry (pi's
  //   allowInvalidThinkingLevelFallback); resolvable base → ok + warning on the result,
  //   still-unresolvable → the retried no-match/ambiguous, with suffixHint naming the bad suffix
  // ④ Literal ids containing a colon (OpenRouter ":exacto") match at ② before ③ ever fires

// ---------- extension/src/subagent/types.ts (M3 core types) ----------
export type ChildStatus = "pending"|"running"|"completed"|"failed"|"interrupted";
export interface ChildResult {
  status: "completed"|"failed"|"interrupted";
  text: string;                 // getLastAssistantText() or "(no output)"
  error?: string;
  attempts?: number;            // total generations (1 + resumes + stall retries); omitted when 1
  stalls?: number;              // stall detections in this user turn (only present if >0)
  durationMs: number;
}
export interface ChildRunRequest {
  childId: string;              // allocated by registry: "ch_" + 8
  runId: string;                // "run_" + 8
  name: string;                 // display name (tasks[].name, agent name, or ordinal)
  prompt: string;               // already interpolated
  agent: AgentDefinition;       // already resolved
  model?: string;               // subagent() parameter-level override
  timeoutMs: number;
  depth: number;                // main session = 0, child = 1
}
export interface ChildHandle {
  readonly childId: string;
  readonly result: Promise<ChildResult>;          // resolves exactly once at terminal state
  steer(message: string): Promise<void>;          // running; terminal → throw
  followUp(message: string): Promise<void>;       // same, queued delivery
  resume(message: string): Promise<void>;         // terminal → continue; does result become pending again afterwards? No:
                                                  // resume returns a new Promise<ChildResult> via registry.getResult()
  interrupt(): Promise<void>;                     // abort; result resolves as interrupted
  status(): ChildStatus;
  lastEventAt(): number;                          // for watchdog
}

// Child-session adapter — wrap pi-runtime.ts's dynamic import in this interface; tests use fakes
export interface ChildSessionAdapter {
  prompt(text: string): Promise<void>;
  steer(text: string): Promise<void>;
  followUp(text: string): Promise<void>;
  abort(): Promise<void>;
  waitForIdle(): Promise<void>;
  getLastAssistantText(): string | undefined;
  isStreaming(): boolean;
  subscribe(listener: (event: { type: string }) => void): () => void;
  dispose(): void;
}
export type CreateSessionFn = (req: ChildRunRequest) => Promise<ChildSessionAdapter>;

export interface ChildRunner {
  start(req: ChildRunRequest): Promise<ChildHandle>;
}

// ---------- extension/src/subagent/registry.ts ----------
export interface RunRecord {
  runId: string; kind: "tasks"|"chain";
  children: { childId: string; name: string; agent: string; status: ChildStatus;
              result?: ChildResult; startedAt: number; endedAt?: number }[];
  status: "running"|"completed"|"partial"|"failed"|"interrupted";
  createdAt: number;
}
export interface RunRegistry {
  createRun(kind: RunRecord["kind"]): RunRecord;
  get(runId: string): RunRecord | undefined;
  list(): RunRecord[];
  handle(childId: string): ChildHandle | undefined;      // active and ended (undisposed) children remain reachable
  findChild(runId: string, childIdOrName: string): ChildHandle | undefined;
  lineage(runIdA: string, runIdB: string): boolean;      // M4 lineage check: v1 = same runId
  onTransition(cb: (run: RunRecord) => void): void;      // for fleet widget / notifications
  disposeRun(runId: string): void;
}

// ---------- extension/src/comms/ (M4, depends only on this interface, no subagent implementation imports) ----------
export interface CommsHost {
  // Implemented by subagent registry (wired during integration)
  getChild(childId: string): { handle: ChildHandle; runId: string; name: string; status: ChildStatus } | undefined;
  listChildren(): { childId: string; runId: string; name: string; status: ChildStatus }[];
  sameRun(childIdA: string, childIdB: string): boolean;
  notifySupervisor(content: string): void;               // → NotifyCenter (triggerTurn/steer routing)
}
export interface MailboxEntry {
  ts: number; from: string; to: string;                  // "supervisor" | childId
  kind: "need_decision"|"progress_update"|"send"|"reply"|"broadcast";
  message: string; reply?: string;
}
export interface Comms {
  contactSupervisor(fromChildId: string, reason: "need_decision"|"progress_update", message: string):
    Promise<string>;        // need_decision → wait for reply text; progress_update → immediate "ok"
  reply(toChildId: string, message: string): void;       // no pending waiter → throw
  send(toChildId: string, message: string, delivery: "steer"|"queue"): Promise<void>;
  broadcast(runId: string, message: string, fromChildId?: string): Promise<string[]>;  // return delivered childIds
  pendingRequests(): { childId: string; name: string; message: string; sinceMs: number }[];
  log(runId: string, limit?: number): MailboxEntry[];    // default 20, ring 200/run
}
export function createComms(host: CommsHost): Comms;
```

**Wiring matrix (integration phase; none of the three parties may cross ownership boundaries)**:

| File | M3 agent | M4 agent | M5 agent |
|---|---|---|---|
| `src/subagent/**` | ✅ Owns | ❌ | ❌ |
| `src/comms/**` | ❌ | ✅ Owns | ❌ |
| `src/agents/**` | ❌ | ❌ | ✅ Owns |
| `src/index.ts` | ✅ Register subagent tool+widget | ❌ (wire during integration) | ❌ (wire during integration) |
| `src/notify.ts` `src/format.ts` | ✅ May add `formatSubagentNotification` | ❌ (supervisor notification formatting self-contained in comms/) | ❌ |
| `src/config.ts` | ✅ May add subagent section | ❌ | ❌ |

## 7. Milestones

| | Content | Status |
|---|---|---|
| M1 | manager (start/wait/output/stop/list/events/CLI) + bash override + task_* | ✅ 2026-09-17 |
| M2 | NotifyCenter + monitor tool | ✅ 2026-09-17 |
| M3 | subagent tool (InProcessRunner + tasks/chain + budget-triggered async + fleet widget) | ✅ 2026-09-17 |
| M4 | comms (contact_supervisor / agent_message) | ✅ 2026-09-17 |
| M5 | agent definition system | ✅ 2026-09-17 |

**Integration additions (2026-09-17, all compatible extensions of Appendix B)**:

- `ChildResult.warning?: string` / `ChildSessionAdapter.warning?: string`: channel for nonfatal warnings such as agent-definition model fallback; runner copies from session at settle
- `subagent` action adds `"models"` (no registry/run_id needed); `SubagentToolDeps.listModels` injects the candidate set
- `PiRuntimeDeps.getScopedModels`: whitelist candidate source; `modelCandidates()` exported for listModels reuse
- `CommsWithOrigin.dispose()`: on session_shutdown, resolve orphaned need_decision waiters with the timeout wording (do not leave them hanging)
- `src/comms/registry-host.ts`: CommsHost ↔ SubagentRegistry adapter (added during integration, self-contained F module)
- F's tool-layer routing violations (cross-run/self-send/missing parameters) return `details.ok:false` + error text, do not throw
- Child-session customTools always inject `contact_supervisor` + `agent_message` (child sender); include the `bash` variant only if the agent's tools contain bash
