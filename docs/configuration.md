# Configuration

pi-famulus reads `~/.pi/agent/pi-famulus/config.json`. The directory can be moved with `PI_FAMULUS_HOME` (or `--home` on the CLI). Machine-wide agent limits live in the same file; see [global-capacity.md](global-capacity.md).

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


## Manager binary discovery

The npm package installs the matching native manager through an exact-version optional dependency. When the extension starts the daemon it looks for the executable in this order:

1. Executable `managerPath` from `config.json`.
2. Executable `PI_FAMULUS_MANAGER_PATH`.
3. The exact-version native npm package.
4. Executable `~/.pi/agent/pi-famulus/bin/pi-famulus` (or the equivalent under `PI_FAMULUS_HOME`).
5. Executable `pi-famulus` on `PATH`.

On Windows the file name in steps 4 and 5, and inside the native package, is `pi-famulus.exe`. An arbitrary different-version native npm package is never substituted. Installing or updating the npm package does not move runtime state or copy a binary into the shared home. The `pi-famulus` CLI that npm installs (`npx pi-famulus --help`) always runs its own native package.

## Degraded startup

If the manager is missing or cannot start, the extension warns in the TUI. Bash then runs locally, so auto-backgrounding and manager-backed output and history are unavailable. `task_*` and `monitor` report that they are disabled. In-process subagents still run, but their `bash` tool fails until the manager is available. Fix the installation, or point to a binary with `PI_FAMULUS_MANAGER_PATH` or `managerPath`. If optional dependencies were omitted at install time, reinstall with them enabled.

If the extension is loaded outside pi and pi's bundled `pi-tui` cannot be resolved, a one-time console warning says the interactive `/tasks` views use a reduced text fallback. Load the extension through pi for the full UI.
