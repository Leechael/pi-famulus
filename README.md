# pi-famulus

A pi extension for subagent orchestration, auto-backgrounding bash, monitoring tasks, and agent-to-agent communication. Process management lives in a standalone Rust daemon, `pi-famulus` (machine-wide singleton, session-isolated, exits with the last pi).

## Architecture

```
pi extension (extension/, TypeScript)        pi-famulus (manager/, Rust)
├─ bash override: foreground budget →        ├─ spawn/wait/stop/output engine
│  auto-background                           ├─ session_id namespacing
├─ subagent: parallel tasks / serial chain   ├─ output duality (ring + full log)
├─ monitor: command output → event stream    └─ lifecycle: 0 connections, 5s →
├─ agent_message / contact_supervisor           kill tasks and exit
└─ NotifyCenter: single injection point
   for all async events
```

Design doc (wire protocol, state machines, interface contracts): [docs/design.md](docs/design.md).

## Install

After the first public npm release:

```bash
pi install npm:pi-famulus
```

npm installs the matching exact-version native manager automatically: Linux/macOS × x64/arm64. No Rust compiler, postinstall download, or separate manager install is required. Linux builds are static musl binaries; macOS builds target macOS 13+ (Node/pi runtime requirements also apply). Windows and other architectures are unsupported. Keep optional dependencies enabled. The npm package also exposes `pi-famulus` on its npm bin path (`npx pi-famulus --help`).

Discovery order: executable config `managerPath` → executable `PI_FAMULUS_MANAGER_PATH` → exact-version native npm package → executable home/bin → executable PATH. Installation does not move runtime state or copy into the shared home. Package installation/usage details: [extension/README.md](extension/README.md). CI, five-package publishing, and the one-time npm Trusted Publisher setup: [docs/releasing.md](docs/releasing.md).

### Source build / local trial

Before the first npm release, or to use a separately built manager:

```bash
# 1. Build and install the manager at ~/.pi/agent/pi-famulus/bin/
cd manager && cargo build --release
mkdir -p ~/.pi/agent/pi-famulus/bin
# Atomic replace (new inode). In-place `cp` onto an existing binary breaks
# macOS code-signing and the next run dies with SIGKILL / "killed".
install -m 755 target/release/pi-famulus ~/.pi/agent/pi-famulus/bin/pi-famulus
#    Subsequent same-name upgrades: the same `install` line is enough.
#    A running daemon upgrades itself in place within seconds (same pid,
#    running work kept); `pi-famulus upgrade` does it now and reports the result.

# 2. Load the extension with this manager (local trial recommended first)
PI_FAMULUS_MANAGER_PATH="$HOME/.pi/agent/pi-famulus/bin/pi-famulus" \
  pi -e /path/to/pi-famulus/extension
# For keeps: `pi install <source>`
```

An installed native npm manager takes precedence over home/bin and PATH. To test a separately built manager, explicitly set `PI_FAMULUS_MANAGER_PATH` as above or set `managerPath` in `~/.pi/agent/pi-famulus/config.json` to its absolute executable path. An executable `managerPath` takes precedence over the environment override; update or clear it when using the latter.

**One-time name transition:** this rename is a breaking installation change, not a hot upgrade of a previous installation. Wait for work to finish or stop it, close the sessions using that installation, and wait for its daemon to exit. Reinstall via npm (once published) or the source-build paths above, migrate **configuration only** to `~/.pi/agent/pi-famulus/config.json` (update explicit paths and environment overrides), then reopen sessions. Do not move the runtime state/history tree: records contain absolute output and transcript paths that a directory move does not rewrite. Keep previous history separately if needed. Subsequent compatible upgrades under the same name and home support the in-place upgrades described above; when changing installation method/binary location, restart the sessions rather than assuming an in-place upgrade across different paths.

**Conflict**: the legacy `pi-subagents` package also registers a `subagent` tool. Either `pi remove pi-subagents`, or test with `pi -ne -e ./extension` (note `-ne` suppresses your other extensions too).

**Degraded startup:** if `pi-famulus` is missing or cannot start, the extension warns in the TUI. Bash runs locally (so auto-backgrounding and manager-backed output/history are unavailable); `task_*` and `monitor` report that they are disabled. In-process subagents remain usable. Fix the manager installation or point to a binary with `PI_FAMULUS_MANAGER_PATH` / `managerPath` in config.json. If the extension is loaded outside pi and pi's bundled `pi-tui` cannot be resolved, a one-time console warning explains that interactive `/tasks` views use reduced text fallback; load the extension through pi for the full interactive UI.

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

## Configuration `~/.pi/agent/pi-famulus/config.json`

```json
{
  "foregroundBudgetMs": 20000,
  "managerPath": null,
  "logLevel": "info",
  "subagent": { "budgetMs": 45000, "timeoutMs": 1800000, "overrunRepeatMs": 600000,
                "hardTimeoutMs": 0, "stallMs": 300000,
                "stallRetries": 1, "stallRetryDelayMs": 5000,
                "decisionTimeoutMs": 600000,
                "concurrency": 4, "maxConcurrentChildren": 8, "spawnBudgetPerHour": 32 }
}
```

Timeouts are staggered so they do not fire together:

| Key | Default | Meaning |
|---|---|---|
| `stallMs` | 300000 (5 min) | No session events. Paused during `tool_execution_start`…`end` and while a `need_decision` is pending. Streaming providers emit `message_update` on `thinking_delta` / `text_delta` (pi agent-loop), which resets this. Not every provider streams partial thinking, so 2 min can kill a slow reasoning turn; 5 min is the default. |
| `stallRetries` | 1 | Auto-resumes after a stall: the aborted generation is retried on the same session with a continuation prompt (transcript preserved). `0` restores the pre-fix behavior (settle `failed (stalled)` at once). |
| `stallRetryDelayMs` | 5000 | Pause between the stall abort and the retry prompt. Gives a flaked provider stream time to recover before the retry. |
| `decisionTimeoutMs` | 600000 (10 min) | Parent did not reply to `need_decision`. |
| `timeoutMs` | 1800000 (30 min) | Soft budget per child turn (launch or `resume`). Reaching it does not stop the child: the parent gets a `subagent-overrun` wake with the child's last activity and, if it is waiting on a foreground shell, that shell, and decides (`extend`, steer, `interrupt`). Stall retries do not restart it. |
| `overrunRepeatMs` | 600000 (10 min) | Repeat of the `subagent-overrun` wake while the child stays past its budget. `extend` re-arms the deadline at now + `timeout_ms` (default: the child's spawn budget); steering once the deadline has passed postpones the next reminder; reminders are held while the child waits on a `need_decision` reply. |
| `hardTimeoutMs` | 0 (off) | Opt-in ceiling per child turn that aborts the child (and its running shell) and settles it `interrupted (timeout)`. `extend` does not move it. |

## Manager CLI

Operations manual (every subcommand, fuzzy ids, output formats): **[docs/cli.md](docs/cli.md)**. Everything the TUI shows can also be answered from the CLI:

| Question | Command |
|---|---|
| Is the daemon healthy? | `pi-famulus doctor` (non-zero exit on any failure), `pi-famulus status` |
| What is each pi session doing, and where? | `pi-famulus sessions` (connected sessions: PID, CWD, running/tasks/agents) |
| What is running / just finished? | `pi-famulus ls [-a] [--session P] [--cwd DIR] [--since 10m] [--json]` (what is running, newest first; `-a` adds connected sessions' finished work; KIND, CWD, STATUS, DUR, EXIT, REASON). A session that exits drops its finished rows from `ls` at once, and drops out of `sessions` too once nothing of it is still running; either way its running work stays listed until it ends, and its records stay reachable by id for `goneSessionRetention` (default 24h) |
| Why did this end? What did it print? | `pi-famulus show <id>` (shell, monitor, `ch_…` agent or `run_…`) |
| What did this subagent do? | `pi-famulus agent <ch_id> [-f] [--full]` (live transcript) |
| Which subagent spent how much CPU on which kind of task? | `pi-famulus stats [--by agent\|kind\|agent,kind] [--since 2h]` (tasks, wall, CPU, average cores, unmeasured, killed); `ls` shows CPU and CORES per finished task |
| Why didn't a notification arrive? | `pi-famulus events [-f] [--id X]` (task lifecycle + wake emit/deliver/inject/dedupe/drop; `wake.inject lag_ms` = how long a wake waited before the model saw it) |
| Follow output | `pi-famulus tail <id>`, `pi-famulus log -f <id> [--stderr]` |

Ids are fuzzy (unique prefix/suffix/near-miss). State directory: `~/.pi/agent/pi-famulus/` (`PI_FAMULUS_HOME` / `--home`).

## TUI (interactive mode)

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

```bash
cd manager && cargo test                        # Rust: unit + adversarial + protocol + observability
cd manager && cargo test --features test-clock  # same suites on a manual clock (fast)
cd extension && npx tsc --noEmit && npx vitest run
PI_FAMULUS_INTEG=1 npx vitest run tests/integration/real-manager.test.ts  # TS ↔ real daemon
```

Manual acceptance checklist: [docs/testing-guide.md](docs/testing-guide.md).
