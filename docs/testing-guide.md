# Manual acceptance guide

About 20 minutes. It walks every feature the way a user meets it, and says what you should see. Anything that doesn't match is a bug. Note the step number and run `pi-famulus doctor` and `pi-famulus events --since 10m` to attach to the report.

Automated suites (run first, all must be green):

```bash
cd manager && cargo test && cargo test --features test-clock
cd extension && npm ci && npx tsc --noEmit && npx vitest run   # CI
cd extension && npm run deps && npx tsc --noEmit && npx vitest run   # local: nub (see eval/README.md)
PI_FAMULUS_INTEG=1 npx vitest run tests/integration/real-manager.test.ts
cd eval && npm ci && npm run test:e2e && npm run test:unit      # CI
cd eval && npm run deps && npm run test:e2e && npm run test:unit   # local: nub
```

## 0. Install into an isolated home

Plain `pi` talks to whatever daemon already runs at `~/.pi/agent/pi-famulus`, possibly an older binary with other sessions attached. Test against a separate home instead:

```bash
cd manager && cargo build --release
mkdir -p ~/.pi/agent/pi-famulus-test/bin
install -m 755 target/release/pi-famulus ~/.pi/agent/pi-famulus-test/bin/pi-famulus   # atomic replace (macOS code signing)

# terminal A, from a scratch directory (worker subagents edit files)
PI_FAMULUS_HOME=~/.pi/agent/pi-famulus-test pi -ne -e /path/to/pi-famulus/extension

# terminal B
export PI_FAMULUS_HOME=~/.pi/agent/pi-famulus-test
alias pi-famulus=~/.pi/agent/pi-famulus-test/bin/pi-famulus
pi-famulus events -f
```

`-ne` keeps other extensions (e.g. an installed `pi-subagents`) out of the way. There must be no "pi-famulus unavailable" warning at startup, and `pi-famulus status` should report the version you just built.

## F1. Bash auto-background

| # | Do | Expect |
|---|---|---|
| 1.1 | Ask: "run `sleep 30 && echo done` with bash" | After ~20s the tool row becomes one line `⏵ sh_… running in background · /tasks`. The agent ends its turn instead of polling. The fleet line shows `● 1 shell … /tasks` |
| 1.2 | Wait | A `✓ task …` pill appears, the agent resumes on its own and mentions `done`. The tool row from 1.1 now reads `✓ sh_… finished · exit 0 · 30.0s` |
| 1.3 | Ask it to run three commands in the background that sleep 5, 10, 15s | Three separate wakes. The first two list the others under "still running" (Ctrl+O on the pill) |
| 1.4 | Ask for `false` in the background | `✗` pill with exit 1; the row turns `✗ … failed · exit 1` |
| 1.5 | Ask: "sleep 60 to wait" | Refused with guidance (bare sleep blocked) |

## F2. Monitor

| # | Do | Expect |
|---|---|---|
| 2.1 | Ask for a monitor on `while true; do date; sleep 2; done`, timeout 20s | `› monitor …` pills while events flow (merged while the agent is busy), then a timeout notice. The tool row shows the monitor as a failure if the manager is missing (F7.5) |
| 2.2 | Ask for a monitor on `yes \| head -c 100000000` | Drops are reported (`dropped-lines`), and after sustained saturation the monitor stops itself with a rate-limit notice; it does not wake the model every 2 s until timeout |
| 2.3 | `/tasks` during 2.1 | The monitor is listed; Enter → output tab shows its lines |
| 2.4 | Ask for a monitor on `echo noop` | One event pill with `noop`, then an exit notice right away (not a timeout later). `/tasks` shows it finished; the fleet line never keeps counting it |
| 2.5 | Ask the agent to stop a running monitor | It ends as killed in `/tasks` and drops out of the fleet line at once |

## F3. Subagents

| # | Do | Expect |
|---|---|---|
| 3.1 | "Use two subagents in parallel: one lists files, one sleeps 60 s via bash then reports" | After 45 s the tool row turns into `run run_… · running · /tasks` with one line per child. The fast child's result arrives as a handover wake while the other still runs; the agent continues right away |
| 3.2 | Wait | A `subagent-done` pill with per-child counts; the fleet line drops the children |
| 3.3 | `/tasks` → select a child → Enter | Conversation (no agent preamble), result, info tabs |
| 3.4 | Use a model that fails (e.g. a provider without credits) for a subagent | Child is `✗ failed` with the provider error, in the pill, `/tasks`, and `pi-famulus show ch_…` (`reason model-error`) |
| 3.5 | Ask for a subagent that must ask you a question (it uses `contact_supervisor`) | `? decision for <name>` pill; answer with `/reply <child> <text>`; the child continues |
| 3.6 | Ask a subagent to start another subagent, or a monitor | It reports the tool is unavailable (depth cap 1; children have no `monitor` / `task_*`) |
| 3.7 | Ask a subagent to run a 30 s command | It blocks and returns the output; nothing is backgrounded. The command shows under its run in `/tasks` and in `pi-famulus ls` |
| 3.8 | Start background work in a second pi session (same home), then ask the first agent "what tasks, monitors and subagents are running?" (also "including finished ones") | Only the first session's own work is listed; nothing from the other session |
| 3.9 | Put `{"subagent":{"overrunRepeatMs":30000}}` in the test home's config.json, restart pi, and ask for one subagent with `timeout_ms: 60000` that runs `for i in $(seq 1 150); do echo $i; sleep 1; done` | After ~60 s a `! overrun` pill (Ctrl+O: shell, output size, last output); the child is still running in `/tasks` and its shell is not stopped; `pi-famulus events` shows `agent.overrun`, no `task.stop`. Without an action from the agent another pill comes ~30 s later. If the agent extends, the next pill waits for the new deadline. The command finishes and the result arrives as usual |
| 3.10 | Same, with `"hardTimeoutMs": 90000` added | Overrun pill(s) first, then at 90 s the child is `interrupted`, its shell stopped (`task.stop reason=tool`), and `agent.timeout` is logged |
| 3.11 | Put `{"subagent":{"maxConcurrentChildren":1}}` in the test home's config.json, restart pi. Ask for one subagent that answers at once; when it has finished, ask for a second, async one that runs `sleep 120` via bash, then ask the agent to resume the first one | The resume tool call returns at once and says every subagent slot is busy, so the child is queued; `/tasks` shows it pending. While it waits, the agent still answers you. When the sleep ends the queued child starts, and its result arrives as a wake |
| 3.12 | During 3.11, `pi-famulus events --since 10m` | Every `wake.deliver` with `mode=steer` or `mode=trigger` is followed by a `wake.inject` with the same kind and a `lag_ms`. In the parent session file (`~/.pi/agent/sessions/…`), the wake entries carry `as-of` and `age-ms` on the `<pi-famulus-wake>` tag |

## F4. `/tasks`

| # | Do | Expect |
|---|---|---|
| 4.1 | Open with several items | A bottom sheet over the editor (not at the top of the screen). Grouped list (subagents under their run), coloured glyphs, exit/reason inline, `(i/n)` when it scrolls |
| 4.2 | Type letters | Filters the list (typing never triggers an action) |
| 4.3 | `ctrl+x` on a running item | Inline red confirm; Enter stops it, Esc cancels. On a finished item: a muted "already finished" hint, nothing written to the transcript |
| 4.4 | Tab | Switches active+recent ↔ all |
| 4.5 | Enter on a shell | `1` output · `2` stderr · `3` info (status, exit, reason, times, paths); `f` toggles follow |
| 4.6 | Resize the terminal to 60 and to 40 columns while open | Nothing wraps or overflows; pi does not exit with "Rendered line exceeds terminal width" |

## F5. Fleet line and pills

| # | Do | Expect |
|---|---|---|
| 5.1 | During F1–F3 | One row: `● 1 shell · 1 monitor · alpha 12s   /tasks`, counting only what is still running |
| 5.2 | A task exits, fails or is killed | It drops out of the row at once (the pill and `/tasks` still report it); with nothing running the row disappears |
| 5.3 | Ctrl+O on any pill | Labelled fields (command, exit, duration, preview…), never raw XML |
| 5.4 | CJK text in commands/results | Columns stay aligned |

## F6. CLI (second terminal)

| # | Do | Expect |
|---|---|---|
| 6.1 | `pi-famulus status` · `pi-famulus doctor` | Version/protocol/uptime; doctor all OK, exit 0 |
| 6.2 | `pi-famulus sessions` | Only connected pi sessions, with PID and CWD (never blank) |
| 6.3 | `pi-famulus ls`, then `ls -a` | `ls`: running work only, newest first. `ls -a`: running first, then connected sessions' finished work, newest first. KIND (shell/monitor/agent), SESSION prefix, CWD, STATUS, TIME (an agent's last message), DUR, EXIT, REASON |
| 6.4 | `pi-famulus show <id>` for a shell, a monitor, a `ch_…`, a `run_…` (fuzzy ids ok) | Everything about it; for an agent: model, error, reason, tool calls, result tail |
| 6.5 | `pi-famulus agent ch_… -f` while a child runs | Live transcript; `--full` also shows the preamble |
| 6.6 | `pi-famulus events -f` while running F1 | `task.start`, `task.background`, `task.exit`, `wake.emit`, `wake.deliver mode=…` |
| 6.7 | `pi-famulus output <id> \| head` | No panic on the closed pipe |
| 6.8 | `pi-famulus stop ch_…` | Explains agents run inside pi (stop from `/tasks`) |

## F7. Manager lifecycle and degraded mode

Upgrade checks 7.6–7.8 cover subsequent compatible same-name upgrades, not the one-time breaking name transition. For that transition, finish or stop work, close the previous installation's sessions, wait for its daemon to exit, reinstall, and migrate configuration only; see [README.md](../README.md#install). Do not move its runtime state/history tree, whose records contain absolute output/transcript paths.

| # | Do | Expect |
|---|---|---|
| 7.1 | Start a background `sleep 300`, quit pi | Within ~7 s `pi-famulus status` says not running (exit 1) and the sleep is gone (`pgrep -f 'sleep 300'` empty) |
| 7.2 | Two pi sessions at once, then quit one | Daemon stays; `sessions` and `ls` show only the remaining session at once; `show <id>` of the quit session's task still works |
| 7.2b | Put `{"goneSessionRetention":"1m"}` in `~/.pi/agent/pi-famulus-test/config.json`, restart the daemon (`pi-famulus shutdown` with no pi open), repeat 7.2 | About a minute after quitting, `sessions/<sid>/` of the quit session is deleted and `show <id>` says not found; `manager.log` has `gc: removed` |
| 7.3 | Start a background `sleep 300 & sleep 300` (a task with a grandchild), then `kill -9` the daemon | Within ~3s both sleeps are gone (`pgrep -f 'sleep 300'` empty). The extension reconnects to a fresh manager; the agent gets an exit wake with status `orphaned`; `ls -a` shows the task `orphaned`, REASON `manager-crash` |
| 7.4 | Background a command that spawns `sleep 300 &` and exits; quit pi | The grandchild `sleep 300` is gone too |
| 7.6 | While 7.3-style work runs (a background `for i in $(seq 1 600); do echo line-$i; sleep 0.5; done`, a monitor on `while true; do date; sleep 1; done`, and a `sleep 300 & sleep 300`), rebuild with any change and `install` the binary again, then `pi-famulus upgrade` | `upgraded in place: … (pid N, generation 1, … running task(s) kept)` with the SAME pid as before (`pi-famulus status`). No pi session shows an error or a warning; the background task later finishes with its real exit code and its output has every `line-$i` exactly once; the monitor keeps delivering; `pgrep -f 'sleep 300'` still shows both sleeps |
| 7.7 | Just `install` a rebuilt binary again, no `upgrade` | Within ~5s `pi-famulus status` shows `upgrades: N (last: … binary-changed, …)` with N one higher; nothing interrupted |
| 7.8 | `install` a broken file (e.g. `echo junk > x; install -m 755 x …/pi-famulus`), then `pi-famulus upgrade` | `upgrade not done, still running …`; everything keeps working. Restore the real binary afterwards |
| 7.5 | Move the manager binary away, start pi | Warning lists the paths tried and says children lose bash; bash still runs locally; monitor/`task_*` report disabled (red); `/tasks` says the manager is unavailable |

## Prompt eval (optional, costs model credits)

See [eval/README.md](../eval/README.md). The smoke tier runs one model × 8 scenarios × 3 repeats (about $4 at list price); pick models in `eval/models.json`, authentication comes from your `pi` login.
