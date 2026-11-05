# Configuration reference

pi-famulus uses one configuration file:

```text
~/.pi/agent/pi-famulus/config.json
```

Set `PI_FAMULUS_HOME` to move the Famulus home directory, or pass `--home` to the CLI. Changes made by the extension require an extension reload. Daemon-owned settings require a daemon restart; `pi-famulus config set` applies capacity changes immediately.

## Famulus settings

| Key | Default | Used by | Description |
|---|---|---|---|
| `foregroundBudgetMs` | `20000` | Extension | How long `bash` waits before moving a command to the background, in milliseconds. |
| `subagentBudgetMs` | `45000` | Extension | Default time a `subagent` call waits before running in the background. `subagent.budgetMs` overrides it. |
| `managerPath` | `null` | Extension; `doctor` | Explicit manager executable. |
| `logLevel` | `"info"` | Extension | Accepted values: `debug`, `info`, `warn`, `error`. Currently parsed but not otherwise used. |
| `maxWarmAgents` | `4` | Daemon | Maximum number of warm hosted agents. |
| `goneSessionRetention` | `"24h"` | Daemon | Retention for files belonging to disconnected sessions. |
| `finishedTaskRetention` | `"24h"` | Daemon | Retention for finished task records and output. |
| `maxAgents` | `8` | Daemon | Machine-wide concurrent subagent limit. |
| `maxTestSuite` | `2` | Daemon | Concurrent test-suite subagent limit. |
| `maxTest` | `2` | Daemon | Concurrent test subagent limit. |
| `maxBuild` | `maxAgents` | Daemon | Concurrent build subagent limit. |
| `maxLintType` | `maxAgents` | Daemon | Concurrent lint/type subagent limit. |
| `maxOther` | `maxAgents` | Daemon | Concurrent general subagent limit. |
| `maxGit` | `maxAgents` | Daemon | Concurrent Git subagent limit. |
| `maxReadSearch` | `maxAgents` | Daemon | Concurrent read/search subagent limit. |

Retention values are durations such as `"30m"`, `"24h"`, `"7d"`, or `"0s"`.

## Subagent settings

These keys belong in the `subagent` object unless noted otherwise.

| Key | Default | Used by | Description |
|---|---|---|---|
| `budgetMs` | `45000` | Extension | Per-call backgrounding budget in milliseconds. |
| `timeoutMs` | `1800000` | Extension | Soft per-turn budget; reaching it does not stop the child. |
| `overrunRepeatMs` | `600000` | Extension | Interval between repeated overrun notifications. |
| `hardTimeoutMs` | `0` | Extension | Optional per-turn abort limit; `0` disables it. |
| `stallMs` | `300000` | Extension | Inactivity watchdog interval, paused while tools run or a decision is pending. |
| `stallRetries` | `1` | Extension | Automatic retry count after a stall. |
| `stallRetryDelayMs` | `5000` | Extension | Delay before retrying a stalled child. |
| `decisionTimeoutMs` | `600000` | Extension | Maximum wait for a parent response to a decision request. |
| `concurrency` | `4` | Extension | Default concurrency within one subagent call. |
| `maxConcurrentChildren` | `8` | Extension | Concurrent children allowed in this Pi session. |
| `spawnBudgetPerHour` | `32` | Extension | Maximum child sessions started per hour in this session. |
| `piCommand` | `null` | Extension | Pi executable, shell command, or JSON argv array for hosted children. `PI_FAMULUS_PI` can also set it. |
| `idleMinMs` | `120000` | Daemon | Minimum warm idle lifetime before parking. |
| `idleGraceMs` | `15000` | Daemon | Grace period after the estimated prompt-cache expiry. |
| `idleMaxMs` | `3600000` | Daemon | Maximum warm idle lifetime. |

## Cache warming settings

These keys belong in the `cacheWarming` object. Warming is off by default.

| Key | Default | Used by | Description |
|---|---|---|---|
| `enabled` | `false` | Extension | Keep the provider prompt cache warm while the session sits idle behind a persistent monitor. |
| `maxIdleMs` | `21600000` (6 h) | Extension | Stop warming this long after the last real request. |

```json
{ "cacheWarming": { "enabled": true, "maxIdleMs": 21600000 } }
```

A persistent monitor can wake a session hours after its last turn. By then the provider prompt cache has expired, and the whole context is written again at full price. Pi's own cache warmer stops 30 minutes after the last request, so it does not cover this.

With `enabled: true`, while the agent is idle and at least one persistent monitor runs, pi-famulus replays the last real session request with a one-token output cap. It sends the replay at `min(0.9 x TTL, TTL - 10s)` after that request, then the same interval after each successful replay. While pi-famulus warms, it answers pi's `cache_warming_decision` with `stop`, so only one warmer runs. Every replay is logged (`cache-warm.sent` in the extension event log) and appended to the session as a `famulus-cache-warm` custom entry. Each entry holds the token usage and cost of that replay, plus the running count and total cost. Custom entries are never sent to the model.

**TTLs come from pi, not from pi-famulus.** pi-famulus warms only models that declare `promptCache` in pi's `models.json`, for example through `modelOverrides`; see pi's [Prompt Cache Lifetimes](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md#prompt-cache-lifetimes). The tier follows pi: `long` when `PI_CACHE_RETENTION=long`, otherwise `short`. pi-famulus ships no TTL table and never edits pi's config.

The first time a persistent monitor starts while the current model has no TTL for the active tier, or while warming is off, an interactive session shows one info notice explaining how to turn warming on. It shows at most once per session and model, and never enters the model's context.

Warming stops when:

- the agent starts a new run or sends a real request (an in-flight replay is aborted; warming reschedules from the new request once the agent settles)
- no persistent monitor remains
- `maxIdleMs` has passed since the last real request
- the model or thinking level changes, the session is compacted, the session tree is navigated, or the session restarts, reloads or shuts down
- a replay misses the cache (no cache read, or more cache write than read), fails, or its timer fires after the safe refresh point; warming then stays off until the next real request

Supported request APIs and the output-cap field each one uses:

| API | Field |
|---|---|
| `anthropic-messages` | `max_tokens` |
| `bedrock-converse-stream` | `inferenceConfig.maxTokens` |
| `openai-completions` | `max_completion_tokens` or `max_tokens`, whichever the provider uses |
| `openai-responses`, `azure-openai-responses` | `max_output_tokens` (16, the API minimum) |
| `mistral-conversations` | `maxTokens` |

pi-famulus does not warm `openai-codex-responses`, `google-generative-ai`, `google-vertex`, `pi-messages`, virtual or custom APIs, or requests that use budget-based thinking (`thinking.type: "enabled"`), an Anthropic `thread`, or `previous_response_id`. Pi's own warmer still applies to those, within its 30-minute limit.

## Messaging bridge settings

These keys belong in the `messaging` object in the same `config.json`. Messaging is disabled unless `messaging.enabled` is explicitly `true`; setting a server or token alone does not enable it. While disabled, the plugin registers no messaging tools (`SendMessage`, `ListSessions`, documented in [tools.md](tools.md)), commands, prompts or completion, and the daemon starts no bridge. `PI_FAMULUS_NO_INTERCOM` also disables messaging. Other messaging settings have no environment-variable or alternate-file overrides. Reload the extension after changing `enabled`; daemon-owned settings are reloaded when supervision starts and otherwise require a daemon restart.

```json
{
  "messaging": {
    "enabled": false,
    "server": "https://127.0.0.1:8443",
    "token": "",
    "name": "",
    "tlsInsecure": false,
    "sock": "/run/user/1000/pi-famulus/client.sock",
    "stateDir": "~/.pi/agent/pi-famulus/messaging",
    "mcpListen": "",
    "mcpToken": "",
    "mcpAllowNonLoopback": false,
    "remoteMessages": "allow",
    "localOnly": false,
    "eventLogSize": 4096,
    "endpoints": []
  }
}
```

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | Opt in to the plugin messaging features and daemon bridge. |
| `server` | `"https://127.0.0.1:8443"` | Relay server URL. |
| `token` | `""` | Enrollment token for `intercom enroll`, or an explicit device credential. A saved device credential for the same server takes precedence during normal bridge connections. Old shared tokens are not accepted by hosted servers. |
| `name` | `""` | Device name used during enrollment and shown by `/famulus` and `pi-famulus top`; the hostname when empty. |
| `tlsInsecure` | `false` | Disable normal relay TLS certificate verification. |
| `sock` | See below | Local bridge socket path. |
| `stateDir` | `~/.pi/agent/pi-famulus/messaging` | Device identity and bridge state directory. |
| `mcpListen` | `""` | MCP listener address; empty disables it. |
| `mcpToken` | `""` | MCP bearer token. When empty, the daemon generates and privately stores a stable token. |
| `mcpAllowNonLoopback` | `false` | Explicitly permit a non-loopback MCP listener. |
| `remoteMessages` | `"allow"` | Remote message policy: `allow` or `refuse`. |
| `localOnly` | `false` | Use only local sessions without a relay. Requires `enabled: true`; without this flag, missing enrollment is reported as an error. |
| `eventLogSize` | `4096` | Maximum retained bridge event-log entries. |
| `endpoints` | `[]` | Webhook endpoint objects. |

Each webhook endpoint has:

| Key | Default | Description |
|---|---|---|
| `name` | Required | Endpoint name. |
| `url` | Required | HTTPS or loopback webhook URL. |
| `secret` | `""` | Optional webhook secret. |
| `auth` | `"hmac"` | Authentication method. |

The default socket is `$XDG_RUNTIME_DIR/pi-famulus/client.sock` when `XDG_RUNTIME_DIR` is set; otherwise it is `pi-famulus-<uid>/client.sock` under the system temporary directory.

A non-loopback `mcpListen` requires `mcpAllowNonLoopback: true` and a non-empty `mcpToken`. A webhook with a secret must use HTTPS or a loopback URL.

### Hosted server enrollment

Hosted servers exchange an operator-issued enrollment token for a per-device credential. A legacy shared token such as the old `dev-token` example is not a device credential.

In `/famulus`, open **Messaging**, set **server**, then open **token** and enter the enrollment token. Pressing Enter immediately exchanges it for a device credential; the menu reports success or the server's error. The submitted token is not stored in `config.json`, and a successful exchange removes any old `messaging.token` value. Enrollment works while messaging is disabled and does not enable it. Because the token is not kept, the **token** row shows `enrolled · device <first 8 characters of the device id>` once the device is enrolled and `not enrolled` otherwise; entering a token on an enrolled device re-enrolls it.

The CLI remains available for non-interactive registration:

```bash
pi-famulus intercom enroll --server https://relay.example.com --token <enrollment-token>
pi-famulus intercom device
```

Both paths use the same enrollment implementation. It sends `device_id`, `name` (the hostname when unset), `token` and `client_version` to `POST /v1/enroll`, using the current messaging configuration, including `stateDir` and `tlsInsecure`. The returned device credential is saved privately in `messaging/device.json` (or the configured state directory), bound to that server, and is not printed. Failed exchanges leave existing configuration and credentials unchanged.

For normal use, set `messaging.enabled` to `true` and `messaging.server` to the same server. Leave `messaging.token` empty; the bridge authenticates `/v1/relay` with the saved device credential. A credential saved for a different server is never used. Set `localOnly: true` explicitly if only same-machine messaging is wanted.

`/famulus` distinguishes disabled messaging from connection failures and shows the latest relay error, including HTTP authentication failures. The plugin also reports failures without disabling shell, task or subagent features. Successful reconnection clears the previous connection error. While the relay is `disconnected` or `unavailable`, `/famulus` offers **Reconnect messaging now** (needs a current manager daemon), and the summary line shows a retry countdown. The pi footer shows one short line such as `msg: reconnecting · retry in 12s · <error>`, and a failure produces one notice per outage plus one when it is over, however the error text changes.

## Environment variables

| Variable | Description |
|---|---|
| `PI_FAMULUS_HOME` | Move the Famulus home and its configuration file. |
| `PI_FAMULUS_MANAGER_PATH` | Override the manager executable; `managerPath` has higher priority. |
| `PI_FAMULUS_SHELL` | Select the shell for bash tasks on Windows. |
| `PI_FAMULUS_PI` | Set the Pi command for hosted subagents. |
| `PI_FAMULUS_PAGER` | Select the pager for CLI listings. |
| `PI_FAMULUS_NO_NATIVE_FETCH` | Set to `1` to skip restoring a missing native manager package. |
| `PI_OFFLINE` | Set to `1` to disable native package downloads. |
| `PI_FAMULUS_NO_INTERCOM` | Disable the messaging bridge when present. |
| `HERDR_SOCKET_PATH` | Override the Herdr socket path. |
| `HERDR_SESSION` | Select the Herdr session socket namespace. |
| `HERDR_PANE_ID` | Include Herdr pane metadata when registering a session. |
| `CODEX_HOME` | Codex state root, default `~/.codex`. |
| `CODEX_APP_SERVER_SOCK` | Override the Codex app-server socket; takes precedence over `CODEX_APP_SERVER_SOCKET`. |
| `CODEX_APP_SERVER_SOCKET` | Fallback override for the Codex app-server socket. |
| `CODEX_DESKTOP_IPC_SOCK` | Override the Codex Desktop socket; takes precedence over `CODEX_IPC_SOCK`. |
| `CODEX_IPC_SOCK` | Fallback override for the Codex Desktop socket. |
| `CODEX_THREAD_ID` | Current Codex thread used as the CLI reply or idle-watch target. |

Device credentials are stored separately from ordinary configuration and must not be copied into shared configuration files.
