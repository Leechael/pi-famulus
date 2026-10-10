# pi-famulus

Let pi delegate, and keep working.

pi-famulus adds subagents and long-running bash to [pi](https://pi.dev), the coding agent. Subagents run inside pi. Shell commands and monitors run under a daemon that stops everything they started if it exits or crashes, and results report back when done: no polling. It is for people who run pi on real projects and want to hand off parallel or slow work, such as a test suite, a build, a dev server or a code review, without stalling the conversation.

## Why

Three things go wrong when an agent has to wait. This [field report on building pi tooling](https://dev.to/zangetsu101/the-coding-agent-i-could-shape-around-my-workflow-3c38) runs into all three.

- **A foreground watch blocks the chat.** Monitoring CI or a dev server holds the turn, and new messages queue behind it. Here `bash` moves a command to the background after 20 seconds and the turn goes on.
- **A backgrounded command finishes silently.** The agent has no way to wake and react. Here the result is injected into the conversation when the command exits, and `monitor` does the same for output lines.
- **A subagent says "done" while its work still runs.** Here a subagent's `bash` runs in the foreground until the command exits or times out, and a subagent cannot start subagents or monitors. So when it reports done, nothing it started through pi-famulus is still running, unless a command detached itself (`setsid`) or left a process running with `&`. Such leftovers keep running until they exit or the daemon stops them.

pi's core leaves these to extensions on purpose ([design, section 1](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md#1-background-and-goals)). pi-famulus adds them with a separate daemon (`pi-famulus`, written in Rust) that starts every shell command and monitor, including the ones subagents run, and is their parent. Subagents themselves run inside pi. Bare `sleep` is rejected, so the model waits for the result instead of polling. Whether a model uses a tool depends on how the tool is described, so the wording is tested against real models in [eval/RESULTS.md](https://github.com/Leechael/pi-famulus/blob/main/eval/RESULTS.md).

## Install

```bash
pi install npm:pi-famulus
```

Requires pi 1.0.0 or newer. Native daemon builds ship for Linux x64 and arm64, macOS x64 and arm64 (macOS 13+), and Windows x64. There is no Windows arm64 build. The daemon comes with the npm package as an exact-version optional dependency, so keep optional dependencies enabled. No Rust compiler or extra download is needed.

Do not install pi-famulus together with `pi-subagents`. Both register a tool named `subagent`, and in pi 1.0.0 the extension loaded first keeps that name, so the other one's `subagent` tool is never exposed. Choose one. `pi-background-tasks` registers the `/tasks` command, as pi-famulus does. In pi 1.0.0 two commands with one name are listed as `/tasks:1` and `/tasks:2`. Its tools (`bg_run` and others) have different names, but the two packages overlap in purpose, so choose one of them too. Details: [development](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md#conflicts-with-other-packages).

## Try this first

You do not write tool calls. After installing, ask pi in plain language.

**1. Plan with your strongest model, implement with a cheap one.** Select your strongest model in the pi session. Then save this as `~/.pi/agent/agents/worker.md`:

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

The session model writes the plan. `subagent` runs `worker` by default, and `worker` now uses the model matching `haiku`. The prompt names no model. Notes:

- `model` is matched against the model ids and names pi lets you use (your `enabledModels` setting or `--models`). It must match exactly one model. If it matches none or several, the child falls back to the parent model and says so in a warning. (A `model` passed on a single `subagent` call is stricter: no match or several matches is an error, and the model is told the candidates.)
- Ask pi "which models can subagents use?" to see the choices (it calls `subagent({action: "models"})`).
- Add a thinking level with a suffix, for example `model: haiku:high`.
- A `worker.md` in `<project>/.pi/agents/` overrides the one in your home directory. Without any `worker.md`, children use the parent's model.

**2. Run something slow.**

```text
Run the full test suite and tell me what fails.
```

`bash` waits 20 seconds. If the command is still running, it moves to the background and the turn continues. The output arrives in the conversation when the command exits.

**3. Watch a log or a server.**

```text
Start the dev server and tell me if an error shows up in its output.
```

`monitor` turns each output line into an event and wakes pi when one arrives.

## What you get

### Delegate to subagents

- `subagent` runs up to 10 tasks per call in parallel (`tasks`, 4 at a time by default) or one after another (`chain`, where `{previous}` carries the last result forward).
- If a run takes longer than 45 seconds, it continues in the background. Each finished subagent reports back on its own.
- `subagent({action: "steer" | "resume" | "extend" | "interrupt"})` redirects a running child, continues a finished one with a new message, gives it more time, or stops it.
- Built-in agents are `explorer` (read, grep, find, ls, bash) and `worker` (inherits your tools). Add your own as markdown files in `~/.pi/agent/agents/` or `<project>/.pi/agents/`.
- Subagents load your pi settings, extensions, skills and context files, including MCP servers. They run inside the pi process and cannot start subagents of their own.
- A subagent can send its parent a question with `contact_supervisor` and wait for the answer. You can answer yourself with `/reply <child> <text>`. `agent_message` sends messages between parent and children.
- At most 8 subagents run at once across all your pi sessions on the machine, and the rest queue. Change the limit with `pi-famulus config set max-agents N`. Work labelled `test-suite` or `test` is limited to 2 at a time. Details: [capacity](https://github.com/Leechael/pi-famulus/blob/main/docs/global-capacity.md).

### Run long shell commands

- `bash` moves a command to the background after 20 seconds (`foregroundBudgetMs`), or at once with `run_in_background`.
- When a background command exits, the result is injected into the conversation. Nothing polls.
- `task_list`, `task_output` and `task_stop` inspect and stop background commands. `/tasks` shows them live.
- A fleet line under the editor lists the shells, monitors and subagents that are running.

### Watch output

- `monitor` runs a command and turns each output line into an event (batched every 200 ms, lines cut at 500 characters, at most 10 events per 2 seconds; batches over the limit are dropped). The command exiting and a timeout also produce events. If at least half the batches are dropped over a full 30-second window containing at least 10 batches, the monitor is stopped and you are told.

### When things go wrong

- A subagent still running when its 30-minute turn budget ends is not killed. The parent is told and chooses to extend, steer or interrupt it.
- A subagent with no activity for 5 minutes is resumed once on the same session with its transcript kept. The timer pauses while a tool is running or a `contact_supervisor` question awaits an answer. If it stalls again, it is marked failed.
- The daemon is the parent of every shell and monitor task. If the daemon crashes or is killed with `kill -9`, each task's process group is stopped. A command that moves a child into a new session (`setsid`) escapes this.
- Running work does not survive the last pi session closing. Five seconds after the last session disconnects, the daemon stops what is left and exits. Records and transcripts stay for 24 hours.
- With the daemon binary installed in `~/.pi/agent/pi-famulus/bin`, replacing it on Linux or macOS makes the running daemon re-execute itself with the same pid. Running commands keep going and clients reconnect (a 30 to 46 ms gap was measured).
- The `pi-famulus` command inspects what happened: `doctor` (health), `ls` (what is running), `show <id>` (why it ended and what it printed), `agent <id>` (a subagent's transcript), `stats` (CPU per agent), `events` (why a notification did or did not arrive). See the [CLI manual](https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md).

## How it differs from other packages

pi-famulus, `pi-subagents` and `pi-background-tasks` are alternatives. Pick one. These notes come from each package's npm tarball and README, read on 2026-10-10. Check them for current behavior.

- [`pi-subagents`](https://github.com/nicobailon/pi-subagents): choose it if you want ready-made roles (`scout`, `reviewer`, `oracle` and others), saved workflows, `/council`, or background children that run in a detached process. It cannot be installed alongside pi-famulus (same `subagent` tool). In pi-famulus, subagents run inside pi and stop when the last session closes.
- [`pi-background-tasks`](https://github.com/ismailsaleekh/pi-background-tasks): choose it if you want named background shell jobs with output files, a read-only delegated agent, or its multi-model Fusion workflows. It shares the `/tasks` command with pi-famulus and covers the same ground.
- Trade-off: subagents run inside pi, so you cannot attach to a child or take over its session like a tmux pane. You can steer, resume or interrupt it through the parent, answer its questions with `/reply`, and read what it did with `pi-famulus agent <id> -f`, `show <id>` and `top`.
- Choose pi-famulus if you want one daemon that starts and cleans up every shell and monitor process across your pi sessions, a shared cap on concurrent subagents, and a command-line tool for inspecting past runs.

## FAQ

**How do I run a subagent on a cheaper model in pi?**
Put `model: haiku` (or another model id you have access to) in the frontmatter of `~/.pi/agent/agents/worker.md`. The `worker` agent is what `subagent` runs when no agent is named. See "Try this first" above. A single call can also pass `model`, but there a model that matches none or several is an error instead of a fallback.

**Why does my pi bash command hang on long builds?**
With pi-famulus, `bash` stops waiting after 20 seconds and moves the command to the background. The model gets the result when the command exits. Change the limit with `foregroundBudgetMs` in `~/.pi/agent/pi-famulus/config.json` ([configuration](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md)).

**What happens to background tasks when pi exits?**
They stop. The daemon exits 5 seconds after the last pi session disconnects and ends the tasks left running (SIGTERM, then SIGKILL after 2 seconds). Records, output and subagent transcripts stay on disk for 24 hours, and you can read them with `pi-famulus show <id>`.

**Does pi-famulus work on Windows?**
Yes on x64. There is no arm64 build. Replacing the daemon binary in place is Unix-only; on Windows, replace it and restart.

**Can a subagent start its own subagents?**
No. Depth is limited to one level.

**How do I see or stop running work?**
Type `/tasks` in pi. Press `ctrl+x` on an item to stop it. From a terminal, `pi-famulus ls` lists what is running.

## Documentation

- [Tool reference](https://github.com/Leechael/pi-famulus/blob/main/docs/tools.md): parameters, agent files, terminal UI
- [Configuration](https://github.com/Leechael/pi-famulus/blob/main/docs/configuration.md): `config.json`, timeouts, binary lookup, degraded mode
- [CLI manual](https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md)
- [Architecture](https://github.com/Leechael/pi-famulus/blob/main/docs/design.md)
- [Machine-wide capacity](https://github.com/Leechael/pi-famulus/blob/main/docs/global-capacity.md)
- [Development](https://github.com/Leechael/pi-famulus/blob/main/docs/development.md): building from source, tests
- [Releasing](https://github.com/Leechael/pi-famulus/blob/main/docs/releasing.md)

MIT licensed.
