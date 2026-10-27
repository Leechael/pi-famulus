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

## Messaging bridge settings

These keys belong in the `messaging` object in the same `config.json`. This is the bridge's only running configuration source; there are no environment-variable or alternate-file overrides. Daemon-owned messaging settings are reloaded when supervision starts and otherwise require a daemon restart.

```json
{
  "messaging": {
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
| `server` | `"https://127.0.0.1:8443"` | Relay server URL. |
| `token` | `""` | Relay or enrollment token. |
| `name` | `""` | Device name used during enrollment and shown by `/famulus` and `pi-famulus top`; the hostname when empty. |
| `tlsInsecure` | `false` | Disable normal relay TLS certificate verification. |
| `sock` | See below | Local bridge socket path. |
| `stateDir` | `~/.pi/agent/pi-famulus/messaging` | Device identity and bridge state directory. |
| `mcpListen` | `""` | MCP listener address; empty disables it. |
| `mcpToken` | `""` | MCP bearer token. When empty, the daemon generates and privately stores a stable token. |
| `mcpAllowNonLoopback` | `false` | Explicitly permit a non-loopback MCP listener. |
| `remoteMessages` | `"allow"` | Remote message policy: `allow` or `refuse`. |
| `localOnly` | `false` | Use only local sessions without a relay. The daemon enables this automatically when no token or device identity exists. |
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
