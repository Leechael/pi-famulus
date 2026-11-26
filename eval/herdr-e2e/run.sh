#!/usr/bin/env bash
# Manual end-to-end run of the MCP start_session / session_status / stop_session tools against a real herdr
# and a real pi. Not for CI. See README.md.
# Isolated: own daemon home, own herdr named session, own pi agent dir. Re-runnable.
# Usage: eval/herdr-e2e/run.sh [scenario keys...]   (see README.md for the keys)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/env.sh"
A=$E/pi-agent

say() { printf '%s [run.sh] %s\n' "$(date +%T)" "$*"; }
die() { say "preflight: $*"; say "nothing was started; see eval/herdr-e2e/README.md"; exit 2; }

# ---- Preflight: read-only checks, before anything is created or started. --------------------------------
[[ $(uname -s) == Linux && -r /proc/self/environ ]] || die "this run needs Linux: it reads /proc"
command -v python3 >/dev/null || die "python3 not found on PATH"
[[ -n $HERDR && -x $HERDR ]] || die "herdr not found: put it on PATH or set PF_E2E_HERDR=/path/to/herdr"
[[ -n $PI && -x $PI ]] || die "pi not found: put it on PATH or set PF_E2E_PI=/path/to/pi"
command -v node >/dev/null || die "node not found on PATH (pi needs it)"
[[ -x $BIN_SRC ]] || die "release binary $BIN_SRC not found: run 'cargo build --release --locked' in manager/, or set PF_E2E_BIN_SOURCE"
[[ -r $PI_AGENT_SRC/auth.json ]] || die "pi credentials $PI_AGENT_SRC/auth.json not found: log in with pi, or set PF_E2E_PI_AGENT_SOURCE"
[[ -r $PI_AGENT_SRC/extensions/herdr-agent-state.ts ]] || die "herdr's pi integration $PI_AGENT_SRC/extensions/herdr-agent-state.ts not found: install herdr's pi integration"
[[ -d $R/extension/node_modules ]] || command -v npm >/dev/null || die "extension/node_modules missing and npm not found to install it"
[[ $PF_E2E_MODEL == */* ]] || die "PF_E2E_MODEL must be provider/model, got '$PF_E2E_MODEL'"
PI_VERSION=$("$PI" --version 2>/dev/null | tail -n 1 | tr -d '[:space:]')
[[ -n $PI_VERSION ]] || die "'$PI --version' printed nothing"
say "preflight: herdr $("$HERDR" --version 2>&1 | head -n 1) at $HERDR; pi $PI_VERSION at $PI; node $(node --version); model $PF_E2E_MODEL"
say "preflight: binary $BIN_SRC ($("$BIN_SRC" --version 2>&1 | head -n 1)); work dir $E"
# Nothing of ours may be running already.
if "$HERDR" session list 2>/dev/null | grep -q "^$HSESS .*running"; then die "herdr session $HSESS already running; stop it ('$HERDR --session $HSESS server stop') or set PF_E2E_HERDR_SESSION"; fi
if "$BIN_SRC" --home "$PFH" status >/dev/null 2>&1; then die "a daemon already runs in $PFH; stop it ('$BIN_SRC --home $PFH shutdown') or set PF_E2E_WORK"; fi

STAMP=$(date +%Y%m%d-%H%M%S)
export RUNSCR=$SCR/run-$STAMP
mkdir -p "$RUN" "$RUNSCR"
: > "$RUN/pids.txt"
rm -f "$RUN/serve.pid" "$RUN/herdr.pid"   # never act on a previous run's pids

# stop_pid PID PATTERN: SIGTERM the exact pid only after printing its command line and matching it.
stop_pid() {
  local pid=$1 pat=$2 args
  args=$(ps -o args= -p "$pid" 2>/dev/null) || { say "pid $pid already gone"; return 0; }
  say "pid $pid: $args"
  if [[ $args == *$pat* ]]; then
    kill -TERM "$pid" && say "sent SIGTERM to $pid"
    for _ in $(seq 1 50); do ps -p "$pid" >/dev/null 2>&1 || break; sleep 0.1; done
    ps -p "$pid" >/dev/null 2>&1 && say "pid $pid still running after 5s; LEFT RUNNING: $(ps -o pid,args= -p "$pid")"
  else
    say "pid $pid does not match '$pat'; not touching it"
  fi
}

teardown() {
  set +e
  say "teardown"
  if [[ -s $RUN/serve.pid ]]; then stop_pid "$(cat "$RUN/serve.pid")" "$BIN --home $PFH serve"; fi
  [[ -x $BIN ]] && "$BIN" --home "$PFH" shutdown 2>&1 | sed 's/^/  shutdown: /'
  sleep 2
  "$BIN_SRC" --home "$PFH" status >/dev/null 2>&1 && say "daemon STILL RUNNING" || say "daemon not running (status exit 1)"
  if "$HERDR" session list 2>/dev/null | grep -q "^$HSESS .*running"; then
    "$HERDR" --session "$HSESS" server stop 2>&1 | sed 's/^/  herdr stop: /'
    sleep 1
  fi
  [[ -s $RUN/herdr.pid ]] && { ps -p "$(cat "$RUN/herdr.pid")" >/dev/null 2>&1 && say "herdr pid $(cat "$RUN/herdr.pid") STILL RUNNING" || say "herdr server pid $(cat "$RUN/herdr.pid") gone"; }
  # Every pi pid the scenarios saw must be dead now.
  for p in $(sort -u "$RUN/pids.txt"); do
    if ps -p "$p" >/dev/null 2>&1; then say "pi pid $p STILL RUNNING: $(ps -o args= -p "$p")"; fi
  done
  # Anything else still carrying this run's daemon home in its environment (listed, never killed).
  left=0
  for d in /proc/[0-9]*; do
    [[ -r $d/environ ]] || continue
    if tr '\0' '\n' < "$d/environ" 2>/dev/null | grep -qx "PI_FAMULUS_HOME=$PFH"; then
      [[ ${d#/proc/} == "$$" ]] && continue
      say "LEFTOVER process with our PI_FAMULUS_HOME: $(ps -o pid=,args= -p "${d#/proc/}")"; left=1
    fi
  done
  [[ $left == 0 ]] && say "no process left with PI_FAMULUS_HOME=$PFH"
  # Credentials copies: delete.
  rm -f "$A/auth.json" "$A/models-store.json" && say "deleted credential copies in $A"
  # herdr named-session directory (outside the work dir).
  "$HERDR" session delete "$HSESS" >/dev/null 2>&1 && say "deleted herdr session $HSESS" || say "herdr session $HSESS not deleted"
  ls -d "$HERDR_SESSIONS_DIR/$HSESS" 2>/dev/null && say "herdr session dir still present"
  say "teardown done"
}
trap teardown EXIT

say "setup: home=$PFH herdr session=$HSESS pi agent dir=$A scratch=$RUNSCR"
"$HERDR" status 2>/dev/null | sed -n '/^server:/,/socket/p' | sed 's/^/  default herdr: /'

# Daemon binary and home.
mkdir -p "$PFH/bin"
install -m 755 "$BIN_SRC" "$BIN"
chmod 700 "$PFH"   # messaging refuses a group/world-writable socket directory
cat > "$PFH/config.json" <<EOF
{
  "managerPath": "$BIN",
  "messaging": {
    "enabled": true,
    "localOnly": true,
    "mcpListen": "127.0.0.1:0",
    "sock": "$PFH/messaging.sock",
    "stateDir": "$PFH/messaging"
  }
}
EOF

# Isolated pi agent dir: the tree's extension, herdr's pi integration, the run's default model.
mkdir -p "$A/extensions"
install -m 600 "$PI_AGENT_SRC/auth.json" "$A/auth.json"
if [[ -r $PI_AGENT_SRC/models-store.json ]]; then
  install -m 600 "$PI_AGENT_SRC/models-store.json" "$A/models-store.json"
else
  say "no $PI_AGENT_SRC/models-store.json; not copied"
fi
cp "$PI_AGENT_SRC/extensions/herdr-agent-state.ts" "$A/extensions/"
write_settings() {   # write_settings EXTENSIONS_JSON_ARRAY
  cat > "$A/settings.json" <<EOF
{
  "quietStartup": true,
  "enableInstallTelemetry": false,
  "lastChangelogVersion": "$PI_VERSION",
  "packages": [],
  "extensions": $1,
  "defaultProvider": "${PF_E2E_MODEL%%/*}",
  "defaultModel": "${PF_E2E_MODEL#*/}"
}
EOF
}
# Preflight, last part: the model must be one this isolated pi can use. Checked with no extension configured
# (nothing of the tree's extension runs), no network, no session. Then the tree's extension is added.
write_settings '[]'
models=$(timeout 60 "$PI" -ne --offline --list-models "${PF_E2E_MODEL#*/}" 2>&1); mrc=$?
if ! awk -v p="${PF_E2E_MODEL%%/*}" -v m="${PF_E2E_MODEL#*/}" '$1 == p && $2 == m { f = 1 } END { exit !f }' <<< "$models"; then
  say "preflight: 'pi --list-models ${PF_E2E_MODEL#*/}' (exit $mrc) with the copied credentials does not list $PF_E2E_MODEL:"
  printf '%s\n' "$models" | tail -n 10 | sed 's/^/  pi: /'
  say "preflight: set PF_E2E_MODEL to a provider/model that 'pi --list-models' shows; herdr and the daemon were not started"
  exit 2
fi
write_settings "[\"$R/extension\"]"
[[ -d $R/extension/node_modules ]] || (cd "$R/extension" && npm ci --no-audit --no-fund)

# herdr server (headless) on its own named session; panes inherit this environment.
cd "$RUNSCR"
nohup "$HERDR" --session "$HSESS" server > "$RUN/herdr-server.out" 2>&1 < /dev/null &
echo $! > "$RUN/herdr.pid"
for _ in $(seq 1 50); do [[ -S $HERDR_SOCKET_PATH ]] && break; sleep 0.1; done
say "herdr pid $(cat "$RUN/herdr.pid"): $(ps -o args= -p "$(cat "$RUN/herdr.pid")")"
(cd "$HERE" && python3 -c 'import lib,json; p=lib.panes(); print("  panes at herdr start:", json.dumps([x["pane_id"] for x in p]))' 2>&1)

# Supervised daemon.
nohup "$BIN" --home "$PFH" serve > "$RUN/serve.out" 2>&1 < /dev/null &
echo $! > "$RUN/serve.pid"
for _ in $(seq 1 100); do "$BIN" --home "$PFH" intercom status 2>/dev/null | grep -q '^mcp_status	running' && break; sleep 0.1; done
say "serve pid $(cat "$RUN/serve.pid"): $(ps -o args= -p "$(cat "$RUN/serve.pid")")"
"$BIN" --home "$PFH" intercom status | grep -E '^(daemon_version|herdr_socket|mcp_status|mcp_url|mcp_token_file|pid|relay|supervised)	' | sed 's/^/  /'

cd "$HERE" && python3 -u run.py "$@"
rc=$?
say "run.py exit $rc"
cat "$RUN/serve.out" | sed 's/^/  serve.out: /'
exit $rc
