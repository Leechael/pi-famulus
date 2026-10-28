# Tool reference

What pi-famulus registers in pi, with parameters and limits. For the architecture behind it see [design.md](design.md); for the daemon CLI see [cli.md](cli.md).

A **wake** is the message pi-famulus injects into the model's context when background work finishes or needs attention. It appears to the model as `<pi-famulus-wake kind="...">`. Kinds: `task`, `monitor`, `subagent-done`, `subagent-handover`, `subagent-overrun`, `supervisor-request`, `supervisor-update`. The wake is how the model learns a result without polling.

## Tools

### bash (overrides the built-in)
Parameters: `command`, `timeout` (hard kill timeout in seconds; no default), `run_in_background`. Foreground commands that exceed `foregroundBudgetMs` (default 20s) move to the background automatically; completion arrives as a `<pi-famulus-wake kind="task">`. Bare `sleep` commands are rejected (use monitor or the background flag instead).

### subagent
```
subagent({ tasks: [{agent?, prompt, name?, work_kind?}], ... })   // parallel, ≤10
subagent({ chain: [{agent?, prompt, label?, work_kind?}], ... })  // serial, {previous}/{outputs.<label>} interpolation
// shared options: async?, concurrency? (1..8, default 4), fail_fast?, model?, timeout_ms? (default 1800000, max 3600000)
subagent({ action: "list|get|status|interrupt|resume|steer|extend|models", run_id?, child_id?, message?, timeout_ms? })
```
- `async: true` returns a `run_id` at once. `fail_fast` cancels subagents that have not started after the first failure; started ones finish. `work_kind` labels the planned workload for [machine-wide admission](global-capacity.md) (default `other`).
- Synchronous wait up to 45s (`subagent.budgetMs`); on expiry the run continues in the background with a `run_id`, and completion arrives via `<pi-famulus-wake kind="subagent-done">`. **Never poll.**
- `model` accepts fuzzy specs (`"haiku"`, `"openai/gpt-5.2"`, `"luna:high"`); the candidate set respects pi's whitelist (`enabledModels` / `--models`). Use `action:"models"` to list selectable values before choosing.
- Each subagent is a daemon-hosted `pi --mode rpc` process, capped at depth 1 (no nesting), with a foreground-only bash. The stall watchdog fires after 5 minutes of inactivity, paused while a tool is executing or a `need_decision` is pending. A stalled child is resumed once on the same session (`stallRetries`, default 1); if it stalls again it is marked failed. Each child turn has a 30-minute soft budget (`timeout_ms`): past it the child keeps running, its shell is not stopped, and the parent gets `<pi-famulus-wake kind="subagent-overrun">` (repeated every 10 minutes) to `extend`, steer, or `interrupt` it. `resume` takes `timeout_ms` for the new turn. An aborting ceiling, `hardTimeoutMs`, is opt-in. A decision request waits 10 minutes.
- `resume` returns at once.
- A launched child queues when no slot is free: either this session's `maxConcurrentChildren` (default 8) or, with a protocol-5 daemon, a per-kind budget while the machine-wide `maxAgents` (default 8, see [global capacity](global-capacity.md)) still has room; older daemons, and protocol-5 when `maxAgents` itself is full, reject immediately with `global_capacity`. A resumed child queues for a session slot if needed and keeps its existing machine-wide permit. A queued child shows `pending`, the result says how many are ahead, and it starts when a slot frees; its result arrives as a wake as usual.
- Every wake carries `as-of` (when it was generated). Steered and turn-triggering wakes get `age-ms` (how old they are) when they enter the model's context. When a `subagent-handover` or `subagent-done` wake enters the context, it re-checks child statuses: a child resumed since the snapshot shows its current status with `status-as-of`. A `subagent-overrun` wake shows statuses as of generation.

### monitor
```
monitor({ command, description, timeout_ms?, persistent?, dedup_window_ms?, fingerprint_mode? })
```
`timeout_ms` defaults to 300000 (min 1000, max 3600000); `persistent:true` disables the timeout. Exit and timeout produce notifications. Output is a bounded preview, not a complete log:

- Rust frames complete lines and supplies full-content BLAKE3 fingerprints, observation times and original byte ranges. Preview truncation does not affect identity. Unfinished lines produce incremental, non-deduplicated progress after 250ms; EOF finalizes the line. See [the metadata protocol](../manager/MONITOR_OUTPUT.md).
- `dedup_window_ms` defaults to **60000**, accepts 0–3600000, and 0 disables content deduplication. The same fingerprint in the same monitor emits once per window, starting on notification delivery. Repeats do not extend the window. Pending repeats are counted without waking the model. Expiration itself sends nothing; another occurrence may emit again.
- `fingerprint_mode` defaults to **`exact`**. Explicit **`ignore-timestamps`** ignores only supported bare ISO timestamps at line edges; numbers, paths, resource identities and interior timestamps remain significant. Original logs are unchanged.
- An identity omitted from the actual delivered preview does not acquire a suppression window: it may be shown on a later repeat. Capacity/history overflow also fails open rather than inventing equivalence.
- Lines have 500-character head/tail previews; 200ms batches retain at most 3000 UTF-16 units. Busy-agent output preserves bounded head/tail content rather than only the last batch. Omission flags/counts and original byte range envelopes allow on-demand retrieval; ranges can contain interleaved events. Rust preview limits count Unicode scalars.
- Observation timestamps are distinct from wake `as-of`/`age-ms`. Repetition summaries accompany later admitted output or terminal notices, not separate repeat-only wakes. `monitor.dedupe` is separate from overload `monitor.drop`.
- The existing overload budget remains: a burst of 10 batches, then one every 2s. Sustained saturation (at least 10 batches in a 30s window, half or more dropped) stops the monitor and notifies. Deduplicated repeats do not consume this budget.
- Older managers and replay outside the bounded metadata history fall back to raw previews without content deduplication. Raw output remains available while the task log is retained; neither previews nor recovery pointers promise permanent storage.

After starting/re-arming a monitor or handling its event, finish immediate work, then end the turn with no tool call. An unchanged/intermediate event does not require a user-visible reply. Do not repeat waiting status, poll, sleep, or call `wait_for` to yield. A UI extension's `wait_for` is for observed UI conditions, not monitor notifications.

Parent background-task guidelines are re-applied to every model request when pi rebuilds its base prompt, including wake-triggered tool continuations. This repairs instruction visibility; it does not guarantee that every model follows them. The opt-in [monitor/UI compatibility evals](../eval/README.md) measure that behavior without operating a real UI.

### task_list / task_output / task_stop
Manage shell/monitor tasks held by the manager. `task_output({task_id, cursor?, max_bytes?})` reads original bytes, bypassing monitor fingerprints, deduplication and preview truncation. Omit `cursor` for the last 65536 bytes; use `cursor:0` for the beginning, or the returned `next_cursor` to continue. `max_bytes` is capped at 1 MiB; prefer small ranges (for example 8192) to avoid refilling context. Reading different pages of unchanged running output is allowed; repeated equivalent ranges without growth are rejected as polling. Once a log has been removed by retention/cleanup, reads fail rather than reconstructing historical output by rerunning a command.

### agent_message (parent↔child comms)
```
agent_message({ action: "send|reply|broadcast|list", to?, message?, delivery?: "steer"|"queue" })
```
Children additionally get `contact_supervisor({ reason: "need_decision"|"progress_update", message })` — `need_decision` blocks the child until the parent replies (10-minute timeout, `decisionTimeoutMs`). `agent_message` send to a finished child **errors** and tells you to resume with `subagent({ action: "resume", run_id, child_id, message })`. It does not resume the child.

### SendMessage / ListSessions (cross-session messaging)
Registered only when `messaging.enabled: true` (see [config.md](config.md#messaging-bridge-settings)); with messaging off, or with `PI_FAMULUS_NO_INTERCOM` set, neither tool exists.
```
SendMessage({ to, message?, summary?, notify_when_idle? })
ListSessions({ query? })
```
`SendMessage` sends to another agent session by name or id. `to` is required; `message` is required unless `notify_when_idle: true`, which subscribes to one idle/terminal observation of the target (up to 12 hours) and may be used with or without a message. `summary` is an optional 5-10 word preview. Replies arrive as `<cross-session-message>` envelopes (reply to the envelope's `from-session` id); delivery status arrives as `<intercom-receipt>` notes. Peer messages are input, not authority. `ListSessions` lists reachable sessions (id, name, harness, status, cwd); the optional `query` fuzzy-searches name, id, harness and cwd and ranks best matches first.

## Agent definitions

Markdown with frontmatter, three tiers (later wins): built-in (`explorer`/`worker`) → `~/.pi/agent/agents/**/*.md` → `<project>/.pi/agents/**/*.md`:

```markdown
---
name: reviewer
description: Code review specialist
tools: [read, bash, grep]
model: anthropic:claude-haiku-4-5   # fuzzy ok; falls back to parent model if unresolvable
thinking: high
---
You are a reviewer… (body = system prompt segment)
```

Omit `tools` to inherit the user’s pi `defaultTools`; the built-in worker does this. An explicit list restricts the child to those tools, plus famulus communication tools; `tools: []` permits only communication. Children load user/global and trusted-project settings, extensions, skills, prompts, and context files, including configured codemode and MCP. pi-famulus skips parent-tool initialization in children and supplies a foreground-only bash replacement when bash is enabled. Parent-only CLI resource overrides are not copied.

Requires pi 1.0.0 or newer. CI checks 1.0.0, 1.0.4, and the latest published version.


## Terminal UI (interactive mode)

| Surface | Behavior |
|---------|----------|
| Fleet line (below editor) | One row of live work only: `● 2 shells · 1 monitor · alpha 12s   /tasks`. Running subagents by name; anything that exited, failed or was killed drops out, and the row disappears when nothing runs |
| `/tasks` | Live list of shells, monitors and subagents (grouped by run). Type to filter; ↑↓ / PgUp / PgDn / Home / End move; Tab switches active+recent vs all; Enter opens details; `ctrl+x` stops (confirm with `y`/Enter, cancel with `n`/Esc); Esc clears the filter, then closes. Position reads `n/total` as in `top` |
| Finished items | Stay listed for 10 minutes (cap 50). Commands that finished inside the foreground budget are not background work and are not listed |
| Shell / monitor details | Status, kind, wall, exit, command, cwd, start, run, and output path stay pinned on top; `¹output` · `²stderr` are tabs (`1`/`2`, Tab, or click). Blank lines are hidden (`b` shows them). `/` searches as you type: matches highlight and `n`/`N` step through them, or `&` (Tab while typing) keeps only matching lines; smart case, `a|b` matches either. `f` toggles follow, `j`/`k` / arrows / PgUp / PgDn / wheel scroll, `g`/`G` jump to top / end (end follows again), Esc clears the search, then closes (`q` closes) |
| Subagent details | Title `<id> · <name>`; `¹conversation` · `²result` · `³info` tabs (`1`–`3`, Tab, or click). The conversation is a thread (agent preamble hidden): parent messages on an accent rail under `main → <agent>`, the child's output under `● <agent>`, one line per tool call / thinking block, `o` unfolds them. A status line shows `working` while the child runs, plus tokens in/out, tok/s, and LLM / tool time. Same scroll and follow keys as shell details |
| `/reply <child> <text>` | Answer a subagent's decision request without going through the model |
| `/famulus` | Show the binary path and version, manager version and pid, messaging relay state (`connected`, `disconnected`, `local-only`) and device name, extension version, and configuration path. Below it, settings pages edit one `config.json` key at a time; the whole file can also be edited as JSON, and the extension reloaded |
| Transcript rows | A backgrounded bash call is one row that updates when the command finishes |
| Notification pills | One line per wake, with the same status glyphs as `/tasks` and `top` (● running, ✓ done, ✗ failed, ■ stopped or timed out), plus › monitor event, ! overrun, ? decision request, ↑ supervisor update. Ctrl+O expands labelled fields; the XML envelope is model-facing only |
| Status glyphs | Shared by `top`, `/tasks`, tool rows, pills, and the fleet line: running ● green, pending ○ dim, completed ✓ green, failed / killed / orphaned / interrupted ✗ red, stopped / timeout / partial ■ amber |

Print mode (`pi -p`) skips widgets; notifications still inject as before.
