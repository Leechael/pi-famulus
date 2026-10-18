# Subagents hosted by the daemon as `pi --mode rpc` processes

**Date:** 2026-10-17  
**Status:** Implemented. This file is the design the code follows.  
**Scope:** `manager/` (new `agent` task kind, RPC relay, hosting registry, retention, roles), `extension/src/subagent/` (RPC runner replaces `InProcessRunner` and `pi-runtime.ts`), `extension/src/comms/` (routing through the daemon), a child mode of the extension, `/tasks` and `pi-famulus top` (live attach), `docs/design.md` §1, §2, §3.2, §3.3, §4.6, §4.7, §6, `docs/global-capacity.md`.

---

## 1. Goals and non-goals

Subagents stop running inside the main agent's pi process. Each subagent is a `pi --mode rpc` process started and owned by the `pi-famulus` daemon, with a normal pi session file. This makes the following possible:

1. The main agent keeps talking to a subagent across many turns, whether the subagent is running or idle.
2. The user switches over and talks to a subagent directly, from the pi TUI and from `pi-famulus top`. Every subagent has a pi session id.
3. Steering a running subagent.
4. Resuming and reusing a subagent by its session id, including from a later parent session.
5. A subagent can be idle: it stays addressable, costs no tokens, and keeps its prompt cache while that is worth it.

Non-goals:

- **No in-process fallback.** When a hosted pi process cannot be started, the subagent tool reports that and the other tools keep working (§11).
- Nesting stays at one level: a subagent cannot start subagents or monitors.
- No cross-machine communication.
- Tool signatures stay as they are (§8). Behavior widens where the old behavior was an error ("child is not running").

## 2. What pi provides

pi-famulus already requires pi ≥ 1.0.0. Every pi feature below exists in 1.0.0; the version column says when it appeared.

| Feature | Since | Used for |
|---|---|---|
| `pi --mode rpc`: JSONL commands on stdin, responses and events on stdout | 0.x | The process boundary |
| `--session-id <id>` creates or reopens a project-local session with that exact id; custom ids are allowed | 0.76 | Child id = session id |
| `--session <path\|id>` reopens a session | 0.x | Respawning a parked subagent |
| `prompt` with `streamingBehavior: "steer"` or `"followUp"`, and a `disposition` (`started` / `queued` / `handled`) in the response | 0.99 | One delivery command for running and idle sessions |
| `steer`, `follow_up`, `abort` (waits until idle), `clear_queue` | 0.x | Steering and the attach view's Esc |
| `agent_settled` event (with `aborted`) | 0.80.4 | "pi will not continue on its own", i.e. idle |
| `get_entries` with a `since` entry-id cursor, stable across restarts | 0.80.3 | Live attach: backfill, then stream |
| `get_state`, `get_session_stats` (tokens, cost, context usage) | 0.x | Status in `/tasks`, `top`, `ls` |
| Model `promptCache: { short, long }` lifetimes in seconds; `PI_CACHE_RETENTION=long`; `cacheWarming: "off" \| "streaming" \| "idle"`; the `cache_warming_decision` extension event; `cacheWrite1h` in usage | 0.86 | Cache expiry estimate and idle retention (§5) |
| `extension_ui_request` / `extension_ui_response` for dialogs from extensions in the child | 0.x | Routed to an attached UI (§9) |
| `--model`, `--thinking`, `--tools`, `--append-system-prompt`, `--name`, `-a` / `-na` (project trust), `-e` | 0.x / 1.0 | Child configuration (§12) |

Two constraints shape the design:

- **pi does not lock session files.** If a second pi process opens a session that a hosted process still has open, both append to one JSONL tree and their in-memory states diverge. A session therefore has exactly one live owner at a time (§6), and "switching to a subagent" goes through the hosted process, never through a second `pi --session`.
- **`ctx.ui.custom()` does not work in RPC mode.** A child extension cannot draw UI. Its dialogs arrive as `extension_ui_request` and need someone to answer.

## 3. Architecture

```
pi session A (extension, role=parent) ──┐
pi TUI /attach (part of a parent) ──────┤                       ┌─ agent ch_1a2b3c4d
pi-famulus top / attach (role=cli) ─────┼── daemon socket ─────►│    pi --mode rpc --session-id ch_1a2b3c4d …
pi-famulus in a hosted pi (role=child) ─┘                       │    process group / Job Object, lifeline
                                                                └─ agent ch_…
```

- **New task kind `agent`.** Started like a shell task: the daemon creates the process group (Job Object on Windows) and the lifeline, and holds stdin and stdout. The runner wrapper passes the daemon's stdin pipe to pi. When the daemon dies, the lifeline kills hosted pi processes like any other task.
- **The daemon relays RPC.** It frames JSONL on LF only (not Unicode separators, see pi `docs/rpc.md`), correlates ids for its own commands, writes inputs from several sources one at a time, and fans events out to subscribers.
- **Connection roles.** `hello` gains `role`: `parent` (today's extension), `cli`, `child`. A `child` connection is the pi-famulus extension inside a hosted pi. It authenticates with a per-agent token the daemon put in the child's environment. It acts inside its parent session's namespace: its shell tasks are attributed to the parent session with `origin.via = "child-bash"`, as today.
- **Child mode of the extension.** pi-famulus in a hosted pi sees `PI_FAMULUS_CHILD_ID`, `PI_FAMULUS_CHILD_TOKEN` and `PI_FAMULUS_HOME`. It registers only `contact_supervisor`, the child `agent_message` and the foreground-only `bash`, adds `CHILD_BEHAVIOR_GUIDELINES`, and reports cache state (§5). This replaces SDK `customTools` injection and the child-role event-bus probe.

### Layering rule (revised)

Today: "the manager handles only processes and output pipes, with no LLM/session semantics." New rule:

> The manager understands pi RPC framing and agent residency: whether a hosted pi is starting, running, idle or gone, from three signals (`agent_start`, `agent_settled`, process exit), plus the retention timer that parks it. It does not interpret conversation content.

Everything else stays in the parent extension: soft budget and overrun wakes, the stall watchdog and auto-resume, wake formatting and injection, model resolution, chains and fan-out. The parent drives these from events the daemon relays.

## 4. Agent state

`ChildStatus` keeps its five values (`pending | running | completed | failed | interrupted`); the registry, agent records, `/tasks` glyphs and the record reader rely on them. Hosting adds two independent fields:

- **`residency`**: `warm` (process alive), `parked` (process exited, session on disk), `none` (never started, or the session was lost).
- **`driver`**: `parent` or `user`. It is `user` while a UI is attached and the user has sent input (§9).

| Event | Status | Residency |
|---|---|---|
| Admitted, process spawned, prompt sent | `pending` → `running` | `warm` |
| `agent_settled` (not aborted) | `completed` (or `failed` if the last assistant message failed) | `warm` |
| `interrupt` / hard ceiling: `abort` | `interrupted` | `warm` |
| Retention ends (§5), parent interrupts an idle child, or the warm cap evicts it | unchanged | `parked`: stdin closed, process exits |
| Any input to a parked child | `pending` → `running` | respawn with `--session <id>` → `warm` |
| Process exits unexpectedly | `failed` (`process-exited`, with the stderr tail) | `parked` if the session file exists, else `none` |

"Idle" means a terminal `ChildStatus` and residency `warm` or `parked`. No idle child is terminal for good: any input continues it.

## 5. Prompt cache and idle retention

**Can we know how much cache time is left?** As an estimate, yes.

- The child-mode extension sees each provider request finish (`message_end` with usage) and knows the model. Lifetime = `model.promptCache.long` when the request used long retention (`PI_CACHE_RETENTION=long`, or `usage.cacheWrite1h > 0`), else `model.promptCache.short`. `cacheExpiresAt = request end + lifetime`.
- A cache-warming refresh (pi `cacheWarming: "idle"` or `"streaming"`) is a provider request too, so it moves `cacheExpiresAt` forward.
- A model without `promptCache` for the active tier reports `unknown`. Providers publish best-effort lifetimes and pi recommends the conservative end, so the value is an estimate, not a guarantee.
- The child sends `agent_cache { expires_at | null, tier, lifetime_ms }` on its child connection whenever it changes. The daemon stores it on the agent, and shows it in `status`, `ls`, `top` and `/tasks`. Wakes and `agent_message list` include it (§8), so the main agent can tell "talk to it now, the cache is still warm" from "starting it again re-reads the whole context".

**Retention (warm → parked).** The daemon parks a warm idle child at

```
max(idleSince + idleMinMs, cacheExpiresAt + graceMs), capped at idleSince + idleMaxMs
```

with defaults `idleMinMs = 2 min`, `graceMs = 15 s`, `idleMaxMs = 60 min`. With an unknown lifetime: `idleSince + idleMinMs`. Because cache warming pushes `cacheExpiresAt` forward, a child with idle warming stays warm until pi stops warming (pi stops when expected savings drop under $0.05) or `idleMaxMs` is reached. Separately, `maxWarmAgents` (machine-wide, default 4) parks the least recently used idle child when exceeded, since every warm child is a Node process.

The same rule applies whether or not the parent session is alive. That covers both "the cache needs attention while the parent is still running" and "what happens when the parent ends":

- **Parent session ends.** Running children are interrupted (`abort`). Then every warm child follows the retention rule, so a user can still attach or a new session can reuse it while its cache is warm. After that it is parked. Parked children are ordinary pi sessions and stay reusable for as long as pi keeps the session file.
- **The child keeps warming the cache after it is parked or orphaned.** The child-mode extension answers `cache_warming_decision` with `stop` once the daemon has scheduled the child to park, so no refresh is paid for a cache that is about to be dropped. Otherwise pi's own decision stands.
- **Daemon lifetime (§3.2) changes.** Today the daemon exits 5 s after the last session disconnects. Now it exits 5 s after the last `parent` disconnects **and** no agent is warm. `child` and `cli` connections do not count.

New config keys (both sides read `config.json`): `subagent.idleMinMs`, `subagent.idleGraceMs`, `subagent.idleMaxMs`, `maxWarmAgents`.

## 6. Sessions and hosting

- **Location:** pi's default session directory (`~/.pi/agent/sessions/<cwd group>/`), or wherever the user's `sessionDir` / `PI_CODING_AGENT_SESSION_DIR` points. The child is started with `--session-id <child_id>` and `--name "<agent>: <display name>"`, so it shows up in `pi -r` under a recognizable name. Child ids keep the `ch_` + 8 format; pi accepts custom ids.
- **Hosting registry:** the daemon keeps `session_id → { child_id, parent_session, residency, pid, session_file, driver }` and persists it in the agent record. `hosted_lookup { session_id | session_file }` answers "is this session hosted, and by whom". `ls`, `show` and `status` print it.
- **Detecting an interactive pi on a hosted session.** pi-famulus in an interactive pi checks the daemon at `session_start` and on `session_before_switch` (`/resume`, `/new`):
  - Hosted and `warm`: in the TUI, offer **Attach** (open the attach view on the hosted process and switch this pi back to its previous session) or **Take over** (the daemon aborts and parks the hosted process; this pi becomes the owner). `session_before_switch` can cancel the switch until the user chooses. At startup (`pi --session ch_…`) the switch has already happened, so the choice is shown before the first prompt. In RPC or print mode: refuse with an error and `ctx.shutdown()`.
  - Hosted and `parked`: no conflict. The daemon records `owner = <that pi session>`, and any input from the parent gets the error "ch_… is open in pi session X; ask the user or wait". When that pi session switches away or ends, ownership returns to the daemon.
  - `hello` gains `session_file`, so the daemon also refuses to respawn a parked child whose file is open in a pi-famulus session.
- **Limitation:** a pi without pi-famulus that opens a hosted session cannot be detected. This is documented.

## 7. Communication

### Parent → child

The parent extension sends `agent_input { child_id, op, text, source }`; the daemon writes the RPC command. `source` is `parent`, `user`, or a sibling `ch_…` and is recorded in `events.jsonl`. If the child is parked, the daemon respawns it first; the input waits for the RPC handshake.

| Tool call | RPC command | Running child | Idle child |
|---|---|---|---|
| `agent_message send` (delivery `steer`, default), `subagent steer` | `prompt` + `streamingBehavior: "steer"` | Delivered before the next model call | Starts a new turn |
| `agent_message send` delivery `queue` | `prompt` + `streamingBehavior: "followUp"` | Delivered when the run ends | Starts a new turn |
| `subagent resume` | `prompt` (new turn, new soft budget) | Error, as today: "still running; use steer" | Starts a new turn |
| `subagent interrupt` | `abort` | `interrupted` | Parks the child now |

A bare `steer` to an idle session is never sent: pi accepts it, but the message is never consumed. A new turn started from an idle child goes through admission (§10).

### Child → parent

The child-mode tools call the daemon on the child connection. The daemon forwards to the parent session's connection, and the parent's `Comms` handles it as today, raising `supervisor-request` / `supervisor-update` wakes.

- `contact_supervisor need_decision` still blocks inside the child's tool call, but now waits on a daemon round-trip: `agent_comms { request_id, reason, message }`, answered by `agent_comms_reply` when the parent calls `agent_message reply`. With no parent connected, the request goes to an attached UI; otherwise it ends with today's 10-minute timeout text.
- Child `agent_message` to a sibling: the daemon checks the lineage rule (same `run_id`) and delivers through the sibling's `agent_input` with `source = ch_…`.
- The mailbox log moves to the daemon (per run, 200 entries), so `list` works from any session that reuses a child.

### Events to observers

`agent_watch { child_id, since_entry? }` subscribes to one agent. The daemon answers with `get_entries` from the cursor, then streams events. A slow subscriber loses `message_update` deltas and gets a `resync` marker (it then reads `get_entries` again); lifecycle events are never dropped. The parent extension subscribes to its own children to drive overrun, stall, transcript and wake logic.

## 8. Tool surface (signatures unchanged)

| Tool / action | Parameters | Change in behavior |
|---|---|---|
| `subagent` `tasks` / `chain` / `async` / `concurrency` / `fail_fast` / `model` / `timeout_ms` / `work_kind` | unchanged | Each child runs in a hosted pi. Results and `details` add `session_id` and `session_file`. |
| `action: "list" \| "get" \| "status"` | unchanged | Also show `residency`, `driver`, cache time left, and who owns the session. |
| `action: "steer"` | unchanged | Also works on an idle child: starts a turn instead of failing. |
| `action: "resume"` | unchanged; `run_id` may be omitted when `child_id` is given | Works on warm (instant) and parked (respawn) children. A child of an ended parent session can be resumed by `child_id`, which is its session id; it is attached to the calling session's registry under its old `run_id`. |
| `action: "interrupt"` | unchanged | Running: `abort`. Idle: park now (frees memory, drops the cache). |
| `action: "extend"` / `"models"` | unchanged | Unchanged. Model resolution stays in the parent (§12). |
| `agent_message` `send` / `reply` / `broadcast` / `list` | unchanged | `send` to an idle or parked child starts a turn instead of failing ("child is not running; use resume"). The result says which happened: "steered into the running child" or "started a new turn on idle child (cache warm, ~3m left)". `broadcast` still reaches running children only. `list` adds residency, driver and cache. |
| `contact_supervisor` | unchanged | Routed through the daemon. |
| `bash` / `monitor` / `task_*` | unchanged | Unchanged. |

**Wakes.** No new kinds.

- Every settle of a turn that the parent started (spawn, `resume`, `steer`, or `send` to an idle child) produces one `subagent-handover` or `subagent-done` wake, following the existing "one wake per settle" rule.
- The `<child>` element and the handover root gain `cache-expires-in-ms?` and `residency?` attributes, and `details` gains `cacheExpiresInMs?` and `residency?` (additive).
- Turns the **user** starts while attached do not wake the parent one by one. On detach, if the user sent anything, the parent gets one `supervisor-update` from that child: "the user talked to this subagent directly", followed by the last assistant text.

**Prompt texts** (`extension/prompts/`): the rules that say "send to an ended child returns an error, use resume" change to say send and resume both continue an idle child. A short line says that an idle child's cache is warm only for the time reported.

## 9. Switching to a subagent (attach)

Both UIs use the same daemon calls: `agent_watch` for the transcript, `agent_input` with `source = "user"`, and `agent_ui_response` for dialogs.

- **`pi-famulus top`**: the agent detail view becomes live (backfill, then stream) with an input line. Enter sends `prompt` (with `streamingBehavior: "steer"` while running). Alt+Enter sends a follow-up. Esc twice sends `clear_queue` then `abort`, and puts the cleared text back in the input. The header shows status, residency, model, context usage and cache time left. `pi-famulus attach <child_id | session_id>` opens `top` focused on one agent.
- **pi TUI**: `/attach <child>`, or `a` on a subagent row in `/tasks`, opens a full-screen `ctx.ui.custom()` view with the same keys. Only in `ctx.mode === "tui"`.
- **Dialogs from child extensions** (`select` / `confirm` / `input` / `editor`) are shown to the attached UI that last sent input, else to any attached UI. With nothing attached, the daemon answers with a cancellation at once and logs `agent.ui_cancelled`, so the child never blocks on a dialog nobody sees. Fire-and-forget calls (`notify`, `setStatus`, `setWidget`) are shown in attached UIs and otherwise dropped.
- **Driver.** The first user input sets `driver = user`. While it is `user`, the parent's overrun reminders for that child are held, the stall watchdog is paused, and the parent's `send` / `steer` results say "the user is talking to this subagent right now" (the message is still delivered). Detaching, or 10 minutes without user input, sets it back to `parent` and sends the summary wake (§8).
- Several UIs may attach at once. The daemon writes their inputs one at a time.

## 10. Capacity

- A machine permit is held per **running turn**, not until a terminal settle. Settling into idle releases it; any new turn (resume, `send`, user input, respawn) acquires it again through the existing v5 queue with the child's `work_kind`. User-started turns are admitted ahead of queued parent-started turns, so attaching never feels stuck.
- `maxWarmAgents` (§5) is a separate memory budget, enforced by parking.
- `docs/global-capacity.md` changes accordingly: the "interrupted but resumable child keeps its permit" rule goes away.

## 11. Finding pi, and what happens when it fails

**Locating the pi command**, in this order:

1. `subagent.piCommand` in `config.json`, or the `PI_FAMULUS_PI` environment variable (an argv array or a path).
2. The running pi: `process.execPath`, plus `process.argv[1]` when it resolves inside the loaded `@earendil-works/pi-coding-agent` package (`import.meta.resolve`). A single-file pi build is just `process.execPath`.
3. SDK hosts, where `argv[1]` is the host application: `process.execPath` plus the loaded pi package's `dist/cli.js`.

The resolved argv is passed to the daemon in `start`. The daemon never searches `PATH`.

**Version check:** the child's hello reports its pi version and pi-famulus version. A child pi below 1.0.0, or one that does not answer `get_state` within 15 s, fails that spawn with a clear error.

**Failure policy:** no in-process fallback. If no pi command can be found, the RPC handshake fails, or the daemon is below protocol 6:

- `subagent` and `agent_message` return a clear error naming the cause and the fix (set `subagent.piCommand`, upgrade the daemon). `/famulus` and `pi-famulus doctor` show the same diagnosis.
- `bash`, `monitor` and `task_*` keep working.
- A capability the child pi lacks (for example no `promptCache` metadata for the model) disables only that feature: the cache shows `unknown` and retention falls back to `idleMinMs`.

## 12. Child configuration

The parent resolves the model as today (`resolveModelSpec` against the parent's registry and scoped models) and passes the result:

```
<pi argv> --mode rpc --session-id <child_id> --name "<agent>: <name>"
          --model <provider>/<id> --thinking <level>
          [--tools <allowlist>] --append-system-prompt <agent prompt file>
          -a | -na                                   # parent's project-trust decision
          -e <this pi-famulus extension>             # see open question 1
cwd = parent cwd
env = parent env + PI_FAMULUS_CHILD_ID, PI_FAMULUS_CHILD_TOKEN, PI_FAMULUS_HOME
```

- Providers that other extensions register in the parent are registered in the child too, because the child loads the same extensions. If the resolved model is still missing in the child, the spawn fails with the model named.
- Steering mode is set to `all` after the handshake, as today.
- The first prompt keeps the "You are running as model …" preamble.
- Agent definitions (`explorer`, `worker`, user and project files) are unchanged.

## 13. Records, transcripts and the CLI

- The pi session file becomes the transcript. The extension stops writing `<home>/sessions/<sid>/agents/<ch>.jsonl`. `pi-famulus agent <id> -f`, `show` and `top` read the session file, or ask the daemon (`get_entries`) when the child is warm.
- The agent record (`<ch>.json`) gains `session_id`, `session_file`, `residency`, `driver`, `cache_expires_at`, `owner_session?` and `pi_version`. Token totals come from `get_session_stats`.
- New `events.jsonl` types: `agent.spawn`, `agent.respawn`, `agent.park { reason: retention | warm-cap | interrupt | parent-end | takeover }`, `agent.input { source, op }`, `agent.attach`, `agent.detach`, `agent.ui_cancelled`, `agent.cache { expires_at }`.
- Protocol level 6: `start` with `kind: "agent"`, `agent_input`, `agent_watch` / `agent_unwatch`, `agent_comms` / `agent_comms_reply`, `agent_ui_response`, `hosted_lookup`, `agent_cache`, and `role`, `session_file`, `child_id` and `token` in `hello`.

## 14. What gets removed

- `InProcessRunner` (`subagent/runner.ts`) as a session driver. Its timer and state logic (`TimerScope`, soft deadline, stall retry, wall-time accounting) moves to the RPC runner, which implements the same `ChildRunner` / `ChildHandle` interfaces over daemon events, so `registry.ts`, `pool.ts`, `tool.ts` and their tests stay largely as they are.
- `pi-runtime.ts`: `createAgentSession`, `createChildResources`, the child-role event-bus probe, `childSessionCreateOptions`. Model candidate listing stays.
- Reload hand-off of live sessions: a `/reload` only re-subscribes to the daemon.
- The README FAQ answers "you cannot attach to a subagent" and "subagents run inside pi" change.

## 15. Phases

1. **Daemon:** `agent` task kind with a stdin pipe, RPC framing and relay, `agent_watch`, residency from events, roles in `hello`, child token, lifecycle counting only `parent` connections, protocol 6. Black-box tests with a fake RPC child (a small script that speaks the protocol).
2. **Parent and child extension:** RPC runner behind `ChildRunner`, child mode, comms through the daemon, pi lookup and errors. Existing tool behavior stays the same in this phase, so current unit tests, `real-manager` integration tests and the eval matrix are the regression bar.
3. **Idle and reuse:** retention with cache estimates, `maxWarmAgents`, park and respawn, permit per turn, the widened `send` / `steer` / `resume` behavior, wake attributes, prompt text changes, the hosting registry and the interactive-pi guard.
4. **Attach:** live `top` view with input, `pi-famulus attach`, pi TUI `/attach`, dialog routing, driver handling and the detach summary.

Docs (`design.md`, `tools.md`, `cli.md`, `configuration.md`, `global-capacity.md`, both READMEs) are updated in the phase that changes the behavior they describe.

## 16. Open questions

1. **Loading pi-famulus into the child.** If the user installed pi-famulus as a package, the child finds it on its own. If the parent loaded it with `-e` (development, eval), the child does not. Passing `-e <own path>` always works but loads it twice when it is also installed. Proposal: always pass `-e`; in child mode, the first instance registers a process-global marker and later instances return. Needs a check of which copy pi loads first.
2. **Running children when the parent ends.** §5 interrupts them. Should a config switch (`subagent.onParentEnd: "interrupt" | "finish"`) let them finish their turn and then follow retention?
3. **User-started turns and admission.** §10 puts them ahead of queued parent turns. Should they bypass the machine budget entirely?
4. **Default `maxWarmAgents` and `idleMaxMs`.** 4 and 60 minutes are guesses. A warm pi process's memory should be measured before choosing.
5. **`parentSession` in the child's session header.** pi records it for forks; `--session-id` cannot set it from the CLI. Is it worth an RPC `new_session { parentSession }` call after spawn (it changes the session id), or is the agent record enough?
