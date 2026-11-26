# Shared environment for the real-herdr run (sourced by run.sh). See README.md for every variable.
# Sourced with `set -u`: every optional variable below has a default.
_here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
R=$(cd "$_here/../.." && pwd)                                        # repo root, from this file's location
E=${PF_E2E_WORK:-$R/eval/.cache/herdr-e2e}                           # all run state lives here (gitignored by eval/.gitignore)
case $E in /*) ;; *) E=$PWD/$E ;; esac
export PF_E2E_MODEL=${PF_E2E_MODEL:-deepseek/deepseek-flash}         # provider/model for every pi started
PI_AGENT_SRC=${PF_E2E_PI_AGENT_SOURCE:-${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}}   # credentials and herdr's pi extension come from here
BIN_SRC=${PF_E2E_BIN_SOURCE:-$R/manager/target/release/pi-famulus}   # the release binary under test
export HERDR=${PF_E2E_HERDR:-$(command -v herdr || true)}
PI=${PF_E2E_PI:-$(command -v pi || true)}
# Panes inherit herdr's environment, so pi (and herdr) must be first on PATH for herdr, the daemon and the panes.
[[ -n $PI ]] && PATH=$(dirname "$PI"):$PATH
[[ -n $HERDR ]] && PATH=$(dirname "$HERDR"):$PATH
export PATH
export PFH=$E/home                       # isolated daemon home
export PI_FAMULUS_HOME=$PFH
export BIN=$PFH/bin/pi-famulus
export PI_FAMULUS_MANAGER_PATH=$BIN
export PI_CODING_AGENT_DIR=$E/pi-agent   # isolated pi config (tree extension + herdr integration)
export HSESS=${PF_E2E_HERDR_SESSION:-pfe2e}
export HERDR_SESSION=$HSESS
HERDR_SESSIONS_DIR=$HOME/.config/herdr/sessions
export HERDR_SOCKET_PATH=$HERDR_SESSIONS_DIR/$HSESS/herdr.sock
export RUN=$E/run
export SCR=$E/scratch
export PYTHONDONTWRITEBYTECODE=1         # no __pycache__ in the source tree
unset PI_FAMULUS_NO_INTERCOM
