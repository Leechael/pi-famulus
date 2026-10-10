# Tool reference

What pi-famulus registers in pi, with parameters and limits. For the architecture behind it see [design.md](design.md); for the daemon CLI see [cli.md](cli.md).

A **wake** is the message pi-famulus injects into the model's context when background work finishes or needs attention. It appears to the model as `<pi-famulus-wake kind="...">`. Kinds: `task`, `subagent-done`, `subagent-handover`, `subagent-overrun`. The wake is how the model learns a result without polling.

## Tools

### bash (overrides the built-in)
Adds a `run_in_background` parameter. Foreground commands that exceed `foregroundBudgetMs` (default 20s) move to the background automatically; completion arrives as a `<pi-famulus-wake kind="task">`. Bare `sleep` commands are rejected (use monitor or the background flag instead).

### subagent
```
subagent({ tasks: [{agent?, prompt, name?}], ... })   // parallel, ≤10, concurrency 1..8
subagent({ chain: [{agent?, prompt, label?}], ... })  // serial, {previous}/{outputs.<label>} interpolation
subagent({ action: "list|get|status|interrupt|resume|steer|extend|models", run_id?, child_id?, message?, timeout_ms? })
```
- Synchronous wait up to 45s (`subagent.budgetMs`); on expiry the run continues in the background with a `run_id`, and completion arrives via `<pi-famulus-wake kind="subagent-done">`. **Never poll.**
- `model` accepts fuzzy specs (`"haiku"`, `"openai/gpt-5.2"`, `"luna:high"`); the candidate set respects pi's whitelist (`enabledModels` / `--models`). Use `action:"models"` to list selectable values before choosing.
- Subagents run in-process via `createAgentSession`, capped at depth 1 (no nesting), with a no-background bash variant. The stall watchdog is 5 minutes of inactivity, paused while a tool is executing or a `need_decision` is pending. Each child turn has a 30-minute soft budget (`timeout_ms`): past it the child keeps running, its shell is not stopped, and the parent gets `<pi-famulus-wake kind="subagent-overrun">` (repeated every 10 minutes) to `extend`, steer, or `interrupt` it. `resume` takes `timeout_ms` for the new turn. An aborting ceiling, `hardTimeoutMs`, is opt-in. A decision request waits 10 minutes.
- `resume` returns at once. When all `maxConcurrentChildren` slots are busy the child is queued (shown `pending`, the result says how many are ahead) and starts when a slot frees; its result arrives as a wake as usual.
- Every wake carries `as-of` (when it was generated). Steered and turn-triggering wakes get `age-ms` (how old they are) when they enter the model's context. `subagent-handover` and `subagent-done` re-check child statuses then: a child resumed since the snapshot shows its current status with `status-as-of`. Re-checking `subagent-overrun` is deferred.

### monitor
```
monitor({ command, description, timeout_ms?, persistent? })
```
Each output line becomes an event (200ms batching, 500 chars/line and 3000 chars/batch caps, 10 events per 2s rate limit). Exit, timeout, and rate-limit saturation all produce notifications.

After starting/re-arming a monitor or handling its event, finish any remaining work, then end the turn with a reply and no tool call. Simply wait for the next notification—do not poll, sleep, or call `wait_for` to yield. A UI extension's `wait_for` is for observed UI conditions, not monitor notifications.

Parent background-task guidelines are re-applied to every model request when pi rebuilds its base prompt, including wake-triggered tool continuations. This repairs instruction visibility; it does not guarantee that every model follows them. The opt-in [monitor/UI compatibility evals](eval/README.md) measure that behavior without operating a real UI.

### task_list / task_output / task_stop
Manage shell/monitor tasks held by the manager.

### agent_message (parent↔child comms)
```
agent_message({ action: "send|reply|broadcast|list", to?, message?, delivery?: "steer"|"queue" })
```
Children additionally get `contact_supervisor({ reason: "need_decision"|"progress_update", message })` — `need_decision` blocks the child until the parent replies (10-minute timeout, `decisionTimeoutMs`). `agent_message` send to a finished child **errors** and tells you to resume with `subagent({ action: "resume", run_id, child_id, message })`. It does not resume the child.


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
| `/tasks` (alias `/bashes`) | Live list of shells, monitors and subagents (grouped by run). Type to filter; ↑↓ / PgUp / PgDn / Home / End move; Tab switches active+recent vs all; Enter opens details; `ctrl+x` stops (inline confirm); Esc closes |
| Finished items | Stay listed for 10 minutes (cap 50). Commands that finished inside the foreground budget are not background work and are not listed |
| Shell / monitor details | `1` output · `2` stderr · `3` info (status, exit, end reason, times, paths). Tab cycles, `f` toggles follow, arrows / PgUp / PgDn / wheel scroll, Esc back |
| Subagent details | Conversation (agent preamble hidden), result, info |
| `/reply <child> <text>` | Answer a subagent's decision request without going through the model |
| Transcript rows | A backgrounded bash call is one row that updates when the command finishes |
| Notification pills | One line per wake (✓ done, ✗ failed, › monitor event, ? decision request). Ctrl+O expands labelled fields; the XML envelope is model-facing only |

Print mode (`pi -p`) skips widgets; notifications still inject as before.
