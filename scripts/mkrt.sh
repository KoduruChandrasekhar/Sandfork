#!/usr/bin/env bash
# Assemble the runtime directory that the daemon binds at /.sandfork/rt in
# every sandbox: the zygote library, its Python wrapper and the kernel.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${SANDFORK_BIN_DIR:-${CARGO_TARGET_DIR:-$HOME/.cache/sandfork/target}/${PROFILE:-debug}}"
mkdir -p "$BIN/rt"
cp python/sandfork_rt/*.py "$BIN/rt/"
cp "$BIN/libsandfork_zygote.so" "$BIN/rt/"
echo "runtime directory: $BIN/rt"
