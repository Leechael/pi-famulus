<div align="center">
  <img src="https://raw.githubusercontent.com/Leechael/pi-famulus/main/assets/logo.png" alt="pi-famulus" width="120" />
  <h1>pi-famulus</h1>
  <p><b>Keep your main agent focused. Hand off the rest.</b></p>
  <p>English · <a href="https://github.com/Leechael/pi-famulus/blob/main/README_CN.md">中文</a></p>
  <p>Subagents and long-running bash for <a href="https://pi.dev">pi</a> and agents built on it. Results wake the agent when work finishes, so the conversation keeps going. One Rust daemon runs the shells and monitors it starts.</p>
  <p>
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/tools.md">Tools</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md">CLI</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/docs/design.md">Architecture</a> ·
    <a href="https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md">Eval results</a>
  </p>
  <a href="https://www.npmjs.com/package/pi-famulus"><img src="https://img.shields.io/npm/v/pi-famulus?style=flat-square" alt="npm"></a>
  <a href="https://github.com/Leechael/pi-famulus/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Leechael/pi-famulus/ci.yml?branch=main&style=flat-square&label=ci" alt="CI"></a>
  <a href="https://github.com/Leechael/pi-famulus/stargazers"><img src="https://img.shields.io/github/stars/Leechael/pi-famulus?style=flat-square" alt="Stars"></a>
  <a href="https://github.com/Leechael/pi-famulus/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square" alt="License"></a>
  <a href="https://github.com/Leechael/pi-famulus/commits"><img src="https://img.shields.io/github/commit-activity/m/Leechael/pi-famulus?style=flat-square" alt="Commits"></a>
  <a href="https://x.com/leechael"><img src="https://img.shields.io/badge/follow-leechael-red?style=flat-square&logo=x" alt="X"></a>
</div>

<!-- TODO(visual): a full-width screenshot or GIF goes here. None exists yet. Candidate: a recording of `/tasks` and the fleet line while a test suite runs in the background and a subagent reports back. -->

## Why

pi-famulus is less about subagents than about the main agent: keep its context clean, keep it on the goal, and keep talking to it while work runs. Plan with the main agent, hand the coding to a subagent on a cheaper model, and let tests and logs run in the background until they need attention.

A famulus is the assistant who runs a scholar's errands, as Wagner does for Faust. pi's core leaves subagents, background bash and monitoring to extensions on purpose ([design, section 1](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md#1-background-and-goals)). pi-famulus adds them. It is for people who run pi, or an agent built on pi, on real projects and want to hand off slow or parallel work, such as a test suite, a build, a dev server or a code review, without stalling the conversation.

Three things go wrong when an agent has to wait:

- **A foreground watch blocks the chat.** With pi-famulus, `bash` moves a command to the background after 20 seconds, and the turn goes on.
- **A backgrounded command finishes silently.** With pi-famulus, the result is injected into the conversation when the command exits, so the model reacts instead of polling.
- **A subagent reports "done" while its work still runs.** With pi-famulus, a subagent's `bash` cannot move to the background, and a subagent cannot start subagents or monitors. When it reports done, nothing it started through pi-famulus is still running, unless a command detached itself with `setsid` or left a process running with `&`.

One machine-wide daemon (`pi-famulus`, written in Rust) starts every shell command and monitor, including the ones subagents run, and is their parent. If the daemon crashes or is killed, the process groups it started are stopped, except commands that moved into a new session with `setsid`. Subagents themselves run inside pi.

pi-famulus is a pi extension, so it works wherever pi loads extensions: the pi TUI, RPC mode, and agents you build with the pi SDK. Outside the TUI there is no `/tasks` list and no fleet line, but results still wake the agent. Print mode (`pi -p`) exits when the first run ends, so results that arrive later are not seen.

### Not a pane manager

If you use Herdr or tmux panes, keep them. Panes are for watching and taking over an agent. pi-famulus is for the main agent: it is woken when a subagent overruns its budget, asks for a decision or finishes, so it never has to watch a pane to know work is done. A subagent that stalls is retried on its own, and the parent hears the outcome.

## Quick Start

> **Before you install**: pi-famulus registers the tools `bash` (replacing the built-in), `subagent`, `monitor`, `agent_message`, `task_list`, `task_output` and `task_stop`, and the commands `/tasks`, `/famulus` and `/reply`. In pi 1.0.0, when two extensions register the same tool name, the extension loaded first keeps it, and the other one's tool is hidden.
>
> - `pi-subagents` also registers `subagent`. Do not install it alongside pi-famulus.
> - `pi-background-tasks` also registers `/tasks`, so pi lists both as `/tasks:1` and `/tasks:2`. It overlaps with pi-famulus in purpose. Choose one of the two.
>
> Details: [conflicts](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md#conflicts-with-other-packages).

```bash
pi install npm:pi-famulus
```

Requires pi 1.0.0 or newer. Native daemon builds ship for Linux x64 and arm64, macOS x64 and arm64 (macOS 13+), and Windows x64 and arm64. The daemon ships as an exact-version optional dependency of the npm package, so keep optional dependencies enabled. No Rust compiler or extra download is needed.

You do not write tool calls. After installing, ask the agent in plain language:

```text
Run the full test suite and tell me what fails.
```

`bash` waits 20 seconds. If the command is still running, it moves to the background and the turn continues. The result arrives in the conversation when the command exits.

## Features

- **Auto-backgrounding bash**: A command that runs longer than `foregroundBudgetMs` (20 seconds by default) moves to the background. Set `run_in_background` to start it there at once. Bare `sleep` is rejected.
- **Wake on completion**: A finished command, a monitor event or a subagent result is injected into the conversation. The agent continues from it without polling.
- **Subagents**: A call takes up to 10 subagents, which run 4 at a time by default, or a `chain` that runs in order. If the wait passes 45 seconds, the call returns and the run continues in the background. Each subagent reports back when it finishes.
- **Clean finish**: A subagent's `bash` runs in the foreground until the command exits or times out, and subagents cannot start subagents or monitors. So a finished subagent leaves nothing it started through pi-famulus running, unless a command detached itself with `setsid` or left a process running with `&`.
- **Hears about trouble early**: With the default settings, a subagent with no activity for 5 minutes is resumed once on the same session, and marked failed if it stalls again. The watchdog pauses while a tool runs or a `contact_supervisor` question waits for an answer. Past its 30-minute soft budget (`timeout_ms`) the child keeps running, and the parent gets an overrun wake every 10 minutes to extend, steer or interrupt it; the wake is held while the child waits for an answer. These values are set in the `subagent` section of the [configuration](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md).
- **One daemon for every shell and monitor**: The daemon is the parent of every shell and monitor task across your pi sessions. If it crashes or is killed with `kill -9`, each task's process group is stopped. Commands that move into a new session with `setsid` escape this.
- **Shared capacity**: By default, at most 8 subagents run at once across all pi sessions on the machine (`pi-famulus config set max-agents <n>` changes it). The rest wait in a queue. Subagents labelled `work_kind: "test-suite"` or `"test"` are capped at 2 at a time; without a label, a subagent counts as `other`.
- **Parent and subagent talk**: A subagent asks its parent a question with `contact_supervisor` and waits for the answer. You can answer it yourself with `/reply`. `agent_message` sends messages between parent and subagents.
- **Inspectable**: `pi-famulus top`, `ls`, `show`, `agent` and `events` show what ran, why it ended, and why a notification did or did not arrive.
- **Tested against real models**: Tool descriptions are graded on real-model runs. The latest matrix (2026-10-08; 12 xAI and OpenAI configurations, 10 scenarios, k=10) recorded 1198 passes and 2 failures, with 6 invalid or errored episodes excluded ([eval/RESULTS.md](https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md)). It predates a later wording change to the `subagent` and wake prompts ([PR #62](https://github.com/Leechael/pi-famulus/pull/62)), which has not been through the full matrix yet.

## Usage Guide

| Ask the agent | What happens |
| :--- | :--- |
| `Run the full test suite and tell me what fails.` | `bash` moves the run to the background after 20 seconds; the result arrives when it exits |
| `Start the dev server and tell me if an error shows up in its output.` | `monitor` batches new output into bounded events; repeated lines are deduplicated within a configurable window |
| `Review these three modules in parallel with subagents.` | `subagent` runs the tasks in parallel; each subagent reports back when it finishes |
| `Which models can subagents use?` | The agent calls `subagent({action: "models"})` and lists the choices |

| In the pi TUI | Does |
| :--- | :--- |
| `/tasks` | Live list of shells, monitors and subagents; `ctrl+x` stops the selected item |
| `/reply <child> <text>` | Answers a subagent's question without going through the model |
| `/famulus` | Shows the `pi-famulus` binary path and the manager and extension versions |
| Fleet line (the status line below the editor) | Shows the shells, monitors and subagents that are running |

**Plan with your strongest model, implement with a cheap one.** Select your strongest model in pi, then save this as `~/.pi/agent/agents/worker.md`:

```markdown
---
name: worker
description: Implements one concrete task end to end
model: haiku
---
Do the task you were given. Make the edits and run the tests that prove it works.
Stay within the task's scope. Report what you changed and how you verified it.
```

Now ask:

```text
Plan the move from callbacks to async/await in src/api/. When the plan is settled,
have subagents implement it one module at a time, then review what they return.
```

The session model writes the plan. `subagent` runs `worker` by default, and `worker` uses the model that `haiku` matches.

- `model` is matched against the model ids and names that pi lets you use: your `enabledModels` or `--models` scope, or every available model when no scope is set. It must match exactly one. In an agent file, no match or several matches fall back to the parent model with a warning. On a single `subagent` call, they are an error, and the model is told the candidates.
- Add a thinking level with a suffix, for example `model: haiku:high`.
- A `worker.md` in `<project>/.pi/agents/` overrides the one in your home directory. Without any `worker.md`, subagents use the parent's model.
- Built-in agents are `explorer` (read, grep, find, ls, bash) and `worker` (inherits your tools). Add your own as markdown files in the same folders.
- `subagent({action: "steer" | "resume" | "extend" | "interrupt"})` redirects a running subagent, continues a finished one, gives it more time, or stops it.

## Reliability

pi-famulus starts and stops processes for you. This section states what it guarantees and where the guarantee ends.

- On Unix, the daemon starts every shell and monitor task in a process group, and is its parent. When the daemon ends (`shutdown`, `kill -9`, panic), each group gets SIGTERM, then SIGKILL 2 seconds later. Windows tasks run in Job Objects and are terminated as a unit when stopped or when the daemon exits.
- A command that moves a child into a new session (`setsid`) leaves the group and escapes this. A process left running with `&` keeps running until it exits or the daemon stops it.
- Running work does not survive the last pi session closing. Five seconds after the last session disconnects, the daemon stops what is left and exits. Records, output and transcripts stay on disk for 24 hours.
- A subagent still running when its 30-minute turn budget ends is not killed, unless you set `hardTimeoutMs`. The parent is told and chooses to extend, steer or interrupt it.
- A subagent with no activity for 5 minutes is resumed once on the same session, with its transcript kept. The timer pauses while a tool runs or a `contact_supervisor` question waits for an answer. If the subagent stalls again, it is marked failed.
- `monitor` batches output every 200 ms. Rust-framed repeated complete lines from the same monitor are deduplicated for 60 seconds by default after a representative is delivered; legacy/raw fallback previews are not content-deduplicated, and missing metadata, capacity eviction, and omitted representatives fail open. Exact matching is the default, with timestamp normalization opt-in. Previews are bounded; original output remains readable with `task_output`. Deduplication does not count as overload loss. Admitted batches allow a burst of 10 events, then one event every 2 seconds. Batches over that limit are dropped. If at least half the batches are dropped over a 30-second window that holds at least 10 batches, the monitor stops and you are told.
- On Linux and macOS, the daemon checks its own executable every 2 seconds. When the file is replaced, the running daemon re-executes itself with the same pid. `pi-famulus upgrade` does the same on demand. Running commands keep going and clients reconnect. The measured gap was 30 to 46 ms.

## FAQ

- **How is pi-famulus different from `pi-subagents` or `pi-background-tasks`?** They are alternatives, so choose one. Choose [`pi-subagents`](https://github.com/nicobailon/pi-subagents) for ready-made roles, saved workflows, `/council`, or background subagents in a detached process. Choose [`pi-background-tasks`](https://github.com/ismailsaleekh/pi-background-tasks) for named background shell jobs with output files, a read-only delegated agent, or its Fusion workflows. Choose pi-famulus for one daemon that owns every shell and monitor process across your pi sessions, a shared cap on concurrent subagents, and a CLI for inspecting past runs.
- **Can I attach to a subagent like a tmux pane?** No. Subagents run inside the agent's process, so you cannot attach to one or take over its session. Steer, resume or interrupt it through the parent, answer its questions with `/reply`, and read what it did with `pi-famulus agent <id> -f`, `show <id>` and `top`.
- **Does it work outside the pi TUI?** Yes. It loads wherever pi loads extensions, including RPC mode (`pi --mode rpc`) and agents built with the pi SDK. Print mode (`pi -p`) loads it too, but exits when the first run ends, so later results are not seen. In an SDK agent, load it through `DefaultResourceLoader` (for example with `additionalExtensionPaths`) and call `session.bindExtensions()`. The real-model eval drives pi in RPC mode.
- **Can a subagent start its own subagents?** No. Nesting is limited to one level.
- **What happens to background tasks when pi exits?** Five seconds after the last pi session closes, the daemon stops whatever is still running and exits. Read the records afterwards with `pi-famulus show <id>`.
- **Does `/reload` stop running work?** No. Shells, monitors and subagents keep running, and their results still arrive. If the reload loads a different pi-famulus version, the old version's work is stopped first.
- **Which binary and versions am I running?** Type `/famulus`. It shows the `pi-famulus` binary path and its version, the running manager's version and pid, and the extension version.
- **Why does my pi bash command hang on long builds?** With pi-famulus, `bash` stops waiting after 20 seconds and moves the command to the background, so the turn continues. To change the limit, set `foregroundBudgetMs` in `~/.pi/agent/pi-famulus/config.json` ([configuration](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md)).
- **How do I see or stop running work?** Type `/tasks` in the pi TUI and press `ctrl+x` on an item. From a terminal, `pi-famulus ls` lists what is running.
- **Does it work on Windows?** Yes, on x64 and arm64. The in-place upgrade, where the daemon re-executes itself, works only on Linux and macOS. On Windows, replace the binary and restart the daemon manually.
- **What if a notification does not arrive?** Run `pi-famulus events --id <id>` to see whether the wake was emitted, delivered and injected. Run `pi-famulus doctor` to check daemon health.

## Docs

- [Tool reference](https://github.com/Leechael/pi-famulus/blob/main/docs/tools.md): parameters, agent files, terminal UI
- [Configuration](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md): `config.json`, timeouts, binary lookup, degraded mode
- [CLI manual](https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md): `doctor`, `ls`, `show`, `top`, `agent`, `stats`, `events`
- [Architecture](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md): daemon, lifeline, wakes, subagent runner
- [Machine-wide capacity](https://github.com/Leechael/pi-famulus/blob/main/docs/global-capacity.md): `max-agents` and work-kind limits
- [Eval results](https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md): real-model scenario matrix
- [Development](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md): building from source, tests, package conflicts
- [Releasing](https://github.com/Leechael/pi-famulus/blob/main/docs/releasing.md)

## Background

pi-famulus borrows several behaviors from other agents. From Claude Code it takes background-on-timeout instead of kill, completion injected into the conversation, a batched `monitor`, and bare-`sleep` interception. From grok-build it takes a 45-second wait budget for subagents and an admission queue that waits instead of rejecting. From lain it takes foreground-only bash for child agents. It also takes lessons from `pi-subagents` and `pi-intercom` about what to leave out. The research notes are in [design, section 1](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md#1-background-and-goals).

## Support

- If pi-famulus helped you, give it a star, or open an [issue](https://github.com/Leechael/pi-famulus/issues) or a pull request.
- When you report a problem, include the output of `pi-famulus doctor` and `pi-famulus events --id <id>`.

## License

Released under the [MIT License](https://github.com/Leechael/pi-famulus/blob/main/LICENSE).
