# herdr-e2e: a manual run against a real herdr

This runs the MCP tools `start_session`, `session_status` and `stop_session` against a real herdr server and real pi agents. It is a verification tool you run by hand. It is not part of CI and no test runner picks it up.

## Why it exists

The contract tests in `manager/tests/intercom_contracts/` talk to a stub herdr. A stub only does what we think herdr does. It cannot show where the real herdr differs: which process_info fields it fills, when it reports an agent, how pi reacts to ctrl+c and ctrl+d, what happens when a user's command takes the pane. Several bugs in these tools were found only by this run. Run it after changing how the daemon starts, tracks or stops a herdr pane.

## What it covers

Each scenario prints its verdicts. Keys in run order:

| Key | Scenario |
|---|---|
| `s1` | tools are listed; a missing or wrong bearer token gets 401 |
| `s2` | start a session (also S3: herdr's agent identity, S4: session_status, S5: stop a fresh session) |
| `s5c` | how pi reacts to ctrl+c and ctrl+d |
| `s6` | stop after one model turn, own pane focused |
| `sA` | stop during a running turn |
| `s7` | herdr's process_info for an idle and a busy shell |
| `sB` | pane with a background job after pi exited |
| `s8` | pi exits by itself |
| `s9` | the pane is taken over by another command or another pi |
| `s10` | the session's pid is killed |
| `s11` | a session not made by start_session is refused; `session rm` works |
| `s12` | argument and name refusals, accepted names |
| `s12b` | an ambiguous name |
| `sR` | ten stops of fresh sessions |
| `sRM` | five `session rm` |
| `sW` | a user command takes the pane while the stop runs |
| `s13` | the client disconnects during start_session |
| `s15` | a message round trip between two sessions |
| `s14` | herdr not running (stops herdr; keep it last) |

Every stop scenario also checks that no other pane changed.

`s7` waits up to 10 seconds for a `❯` in the shell prompt before it types. With another prompt it just waits the full 10 seconds.

## Prerequisites

- Linux. Teardown and several checks read `/proc`.
- herdr 0.9.5 on `PATH`, with its pi integration installed (`extensions/herdr-agent-state.ts` in your pi agent dir).
- pi on `PATH` (tested with 1.5.2), and node for it.
- pi credentials for the model: `auth.json` in your pi agent dir. `models-store.json` is copied too if it exists.
- python3 (standard library only).
- The release binary: `cargo build --release --locked` in `manager/`.
- `extension/node_modules`. If it is missing, the run installs it with `npm ci`.

## Run it

From the repo root:

```bash
mkdir -p eval/.cache
eval/herdr-e2e/run.sh 2>&1 | tee eval/.cache/herdr-e2e.out         # every scenario
eval/herdr-e2e/run.sh s1 s2 sW 2>&1 | tee eval/.cache/herdr-part.out  # selected scenarios, by key
```

The output holds local paths. Keep it out of commits (`eval/.cache/` is gitignored).

With no settings it uses the model `deepseek/deepseek-flash`. Set `PF_E2E_MODEL` if you do not have that one. The model must appear in `pi --list-models`.

| Variable | Default | Meaning |
|---|---|---|
| `PF_E2E_MODEL` | `deepseek/deepseek-flash` | `provider/model` for every pi the run starts |
| `PF_E2E_WORK` | `eval/.cache/herdr-e2e` (gitignored) | where all run state goes |
| `PF_E2E_HERDR` | `herdr` on `PATH` | herdr binary |
| `PF_E2E_PI` | `pi` on `PATH` | pi binary |
| `PF_E2E_PI_AGENT_SOURCE` | `$PI_CODING_AGENT_DIR`, else `~/.pi/agent` | where credentials and herdr's pi extension are copied from |
| `PF_E2E_BIN_SOURCE` | `manager/target/release/pi-famulus` | daemon binary under test |
| `PF_E2E_HERDR_SESSION` | `pfe2e` | herdr named session for the run |

Unix socket paths have a length limit of about 108 bytes. If the daemon fails to bind its sockets, set `PF_E2E_WORK` to a short path such as `/tmp/pfe2e`. The herdr socket is assumed at `~/.config/herdr/sessions/<name>/herdr.sock`.

A full run takes about 5 minutes. Every session is a real pi on the real model, and `s6`, `sA` and `s15` take model turns, so it costs model usage.

## Preflight

Before it creates or starts anything, the run checks for Linux, python3, herdr, pi, node, the release binary, `auth.json` and herdr's pi extension. It also stops if the herdr session is already running or a daemon already runs in its home. Each failure prints one line saying what is missing and exits 2.

After it copies the credentials, it checks that pi lists the model. If not, it exits 2 and removes the copies. herdr and the daemon are not started yet.

## What it creates and removes

Under the work dir:

- `home/`: the daemon home (binary copy, `config.json`, sockets, messaging state).
- `pi-agent/`: an isolated pi agent dir (`PI_CODING_AGENT_DIR`). It holds copies of `auth.json` and `models-store.json`, herdr's pi extension, and a `settings.json` that loads this tree's `extension/`.
- `run/`: pid files and the herdr and daemon logs of the last run.
- `scratch/run-<time>/`: the cwd of every session.

Outside it: a herdr named session (`~/.config/herdr/sessions/<name>/`).

Teardown always runs on exit:

- It stops the daemon by its recorded pid, after printing the command line and matching it. Then it runs `pi-famulus shutdown`.
- It stops herdr with `herdr --session <name> server stop`.
- It reports any pi pid the scenarios saw that is still alive.
- It lists any process that still has this run's `PI_FAMULUS_HOME`. It never kills them.
- It deletes the credential copies.
- It deletes the herdr named session.

The run signals only pids it recorded itself, after checking their command line. It never kills by pattern. The work dir stays for inspection; delete it by hand.

## Reading the output

Each verdict is one line:

- `PASS [scenario] expected: … | actual: …`: the check held.
- `FAIL [scenario] expected: … | actual: …`: the check failed. The `evidence` lines above it show what herdr, pi and the daemon reported.
- `OBSERVED [scenario] …`: a measured fact about herdr or pi, with no pass or fail. Timings and pi's key handling are here. Read them when herdr or pi changes version.

At the end: `SUMMARY` (one line per verdict), stop timings, `session rm` results, the `W` outcomes, every pi pid seen, and a `TOTAL PASS n FAIL n OBSERVED n` line. The run exits 0 when there is no FAIL, 1 when there is one, and 2 when preflight stops it.

Every stop of a live session the run started, every `session rm` and every `W` case has its own verdict line. The tallies at the end summarize the same results. Then the teardown lines follow. Check them too: every one should say gone, deleted or not running.

A clean run of the full set prints 57 PASS, 0 FAIL, 7 OBSERVED. `S15` is a PASS only when the model answers with the messaging tool. If it does not, `S15` is OBSERVED and the total shifts by one.

## Not for CI

It needs a real herdr, Linux, model credentials and paid model calls, and it takes minutes. No workflow calls it, and the `eval/` npm scripts list their files explicitly, so none of them picks it up.
