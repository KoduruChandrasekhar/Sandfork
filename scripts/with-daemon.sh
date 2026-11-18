#!/usr/bin/env bash
# Run a command against a throwaway daemon with its own state directory.
#
#   scripts/with-daemon.sh SOCKET COMMAND [ARGS...]
#
# The daemon is stopped when the command exits, and the command's exit
# status is returned.
set -euo pipefail
cd "$(dirname "$0")/.."
SOCK="$1"
shift
STATE="${SANDFORK_TEST_STATE:-$(mktemp -d "${TMPDIR:-/tmp}/sf-state.XXXXXX")}"
LOG="$STATE/daemon.log"
rm -f "$SOCK"
SANDFORK_STATE_DIR="$STATE" bash scripts/dev-daemon.sh --socket "$SOCK" --wheels-dir "${SANDFORK_WHEELS:-${XDG_DATA_HOME:-$HOME/.local/share}/sandfork/wheels}" >"$LOG" 2>&1 &
PID=$!
cleanup() {
  kill -TERM "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 100); do
  [ -S "$SOCK" ] && break
  if ! kill -0 "$PID" 2>/dev/null; then
    cat "$LOG" >&2
    exit 1
  fi
  sleep 0.1
done
[ -S "$SOCK" ] || { cat "$LOG" >&2; exit 1; }
set +e
"$@"
STATUS=$?
set -e
if [ "$STATUS" -ne 0 ]; then
  echo "--- daemon log ($LOG)" >&2
  tail -20 "$LOG" >&2
fi
exit "$STATUS"
