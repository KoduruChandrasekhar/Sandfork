#!/usr/bin/env bash
# Fill the wheel cache that base layers install from.
#
# Sandboxes have no network, so everything a base needs is downloaded here,
# once, on the host, and bound read-only at /wheels while a base is built.
#
#   scripts/wheels.sh [DIR]      default: $XDG_DATA_HOME/sandfork/wheels
set -euo pipefail
DIR="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/sandfork/wheels}"
PIP="${PIP:-python3 -m pip}"
mkdir -p "$DIR"
# Wheels only, for the interpreter the sandboxes will run (the host's python3).
PYVER="$(python3 -c 'import sys; print(f"{sys.version_info[0]}.{sys.version_info[1]}")')"
$PIP download --quiet --only-binary=:all: --python-version "$PYVER" --dest "$DIR" pytest numpy pandas
echo "wheel cache: $DIR ($(ls "$DIR" | wc -l) files, python $PYVER)"
