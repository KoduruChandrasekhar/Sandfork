#!/usr/bin/env bash
# Run cargo with the build directory on the Linux filesystem.
#
# The sources may live on a Windows-mounted drive (/mnt/c), which is slow and
# cannot hold overlayfs upper directories, so build output and all runtime
# state stay under $HOME.
set -euo pipefail
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/sandfork/target}"
exec cargo "$@"
