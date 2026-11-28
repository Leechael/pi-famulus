# Antigravity CLI as a Famulus MCP client

Antigravity CLI (`agy`) can call the Famulus Streamable HTTP MCP listener.
Connecting does not register the AGY conversation as an intercom session.
In particular, a bearer token authenticates the HTTP request but does not
create a sender or watcher identity.

## Connect over loopback

1. In [Famulus messaging settings](config.md#messaging-bridge-settings), enable messaging and
   set `mcpListen` to a loopback address, for example `127.0.0.1:8765`.
   Start or supervise the manager with `pi-famulus serve`. Set `localOnly`
   if the manager should not use a relay.
2. Use the MCP URL and token file shown by `pi-famulus intercom status --json`,
   or your configured `mcpToken`. The MCP token is not an enrollment token.
3. Configure an AGY MCP server using `url` and a Bearer header. A project-local
   plugin can put this in `.agents/plugins/famulus-local/mcp_config.json` beside
   its `plugin.json` (a manifest with `{"name":"famulus-local"}` is sufficient).

```json
{
  "mcpServers": {
    "famulus": {
      "url": "http://127.0.0.1:8765/mcp",
      "headers": {
        "Authorization": "Bearer <your MCP token>"
      }
    }
  }
}
```

Keep the file that contains the real token private and out of version control.
Use `/mcp`, not a legacy SSE endpoint, and use `url`, not `httpUrl`. AGY also
supports the global `~/.gemini/config/mcp_config.json`; a project-local plugin
does not require editing that file.

## Which tools work?

Observed with AGY **1.5.18** and Famulus **0.2.7** (`17564b4307`) using a private
loopback daemon, a project-local AGY plugin, and synthetic intercom peers. The
global AGY MCP configuration was unchanged. No credential files were copied.

| Tool | Live AGY result | Requirement or limit |
| --- | --- | --- |
| `list_sessions` | Returned both fixture peers with `query` set. | A query is optional. |
| `name_session` | Returned `{"ok":true}` when renaming the fixture recipient. | Acts on the named directory session, not on the AGY conversation. |
| `session_status` | Returned `state: unknown` for the fixture peer with no verified PID. | This is a successful lookup, not a connection error. |
| `send` | With a fixture-delegated identity, returned `delivered`; the recipient socket captured the matching message ID, body, and sender. An unregistered `agy/…` caller ID was refused. | `from` must identify the caller's registered local intercom session. AGY does not register one over MCP. |
| `message_status` | Returned `read` for the same message ID after the recipient's receipt. | No `from` argument; requires a known message ID. |
| `notify_when_idle` | Refused the unregistered `agy/…` watcher; subscribed with the synthetic Pi watcher and sent the idle notice to that fixture peer. | The MCP call uses a registered local watcher in `from`. A `pi_push` notice goes to that Pi connection, not to AGY. |
| `idle_status` | Returned `idle` for the successful subscription's ID. | Poll by subscription ID; no AGY identity is required to query it. |
| `start_session` | Reached the daemon and returned the normal Herdr-unavailable tool error: the probe's supervised daemon did not inherit the fixture Herdr socket. | Successful create is black-box HTTP tested with a Herdr fixture; this tool starts **Pi**, not AGY. |
| `stop_session` | Discovered but not called live; there was no started session handle. | Successful stop is black-box HTTP tested. It only stops a session this daemon created via `start_session`. |

The model reached its quota after the listed live calls. No additional model
runs were made. The successful lifecycle test is
`mcp_transport::started_session_is_alive_once_registered_and_stop_closes_only_its_own_pane`;
it does not use AGY or a model.

**Sender identity is the main gap.** The probes returned
`from "agy/unknown-probe" is not a local intercom session` and
`watcher "agy/unknown-probe" is not a local session`. Do not substitute an
unrelated session ID from the directory. The successful calls used an
explicitly delegated fixture-only identity, not a workaround for real sessions.
Use the exact ID of your own registered local session; the token and AGY's
conversation UUID are not intercom IDs.

<!-- deferred: register each AGY conversation as a local intercom session | impact: an AGY caller has no native sender or watcher identity for send and notify_when_idle | trigger: an AGY session hook plus a registration path that vouches for the conversation, or a native Antigravity adapter, is available -->

## Transport evidence

The following excerpts are from the loopback capture, with tokens redacted
and unrelated fields omitted.

**Discovery and preflight.** The authenticated AGY client selected the modern
protocol without an `initialize` round trip:

```text
POST /mcp  server/discover
Accept: application/json, text/event-stream
Authorization: Bearer <redacted>
MCP-Protocol-Version: 2026-07-28
(params._meta protocol version: 2026-07-28; Mcp-Session-Id: absent)

200 {"result":{"supportedVersions":["2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"], ...}}
POST /mcp  subscriptions/listen  -> 200 text/event-stream
data: {"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":2},"notifications":{"toolsListChanged":true}}}
POST /mcp  tools/list            -> 200 (9 tools)
POST /mcp  tools/call            -> 200
```

Later requests kept the same Accept and protocol-version headers and still did
not carry a session ID. The SSE acknowledgement did not prevent later calls.
No tool-list change or other server-initiated notification was exercised.
This SSE response to a Streamable HTTP POST is not the legacy SSE transport
that AGY does not support.

**Bad token.** A separate plugin entry using an intentionally wrong token
produced:

```text
POST /mcp/unauthorized  server/discover                          -> 401 unauthorized
POST /mcp/unauthorized  initialize (protocolVersion: 2025-11-25) -> 401 unauthorized
GET /.well-known/oauth-protected-resource/mcp/unauthorized      -> 404
GET /.well-known/oauth-protected-resource                      -> 404
```

The fallback initialize request did not carry an MCP protocol-version header.
In print mode AGY omitted the rejected server's tools without displaying a 401
in stdout or stderr; the healthy server was still discovered. The listener uses
a configured Bearer token, not OAuth discovery. Interactive UI error display
was not checked.

**Tool names and schema.** The server returned bare names, e.g. `send`; AGY
loaded them lazily under the plugin/server name `prprobe_fam`. The live call
used AGY's generic wrapper, not nine separate Gemini function declarations:

```text
tools/list:       {"name":"send","inputSchema":{"type":"object","additionalProperties":false, ...}}
AGY lazy schema:  {"name":"send","parameters":{"type":"object","additionalProperties":false, ...}}
AGY tool call:    call_mcp_tool {"ServerName":"prprobe_fam","ToolName":"send","Arguments":{ ... }}
MCP wire:         tools/call {"name":"send","arguments":{ ... }}
```

The local schema retained `additionalProperties: false`; calls were not
rejected by Gemini for that key. This path does not test support for the
keyword in a directly declared Gemini function. All nine names were
discovered at the tested lengths; no maximum-name-length test was run.
