#!/usr/bin/env bash
# Start sandforkd from the build directory, for development.
#
# With systemd available the daemon runs in a delegated scope, which is what
# gives rootless (L1) sandboxes cgroup limits. Extra arguments go to sandforkd.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${SANDFORK_BIN_DIR:-${CARGO_TARGET_DIR:-$HOME/.cache/sandfork/target}/${PROFILE:-debug}}"
RT="${SANDFORK_RT_DIR:-$BIN/rt}"
ARGS=(--bin-dir "$BIN")
[ -d "$RT" ] && ARGS+=(--rt-dir "$RT")
if [ "${SANDFORK_NO_SCOPE:-0}" != 1 ] && systemd-run --user --scope -q -p Delegate=yes true 2>/dev/null; then
  exec systemd-run --user --scope -q -p Delegate=yes "$BIN/sandforkd" "${ARGS[@]}" "$@"
fi
exec "$BIN/sandforkd" "${ARGS[@]}" "$@"
