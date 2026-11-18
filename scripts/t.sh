#!/usr/bin/env bash
# Quiet wrapper used during development: build, then run one cargo command
# and show only what matters.
cd "$(dirname "$0")/.."
bash scripts/cargo.sh "$@" 2>&1 | grep -vE "Cargo.toml|unused workspace|consider removing the workspace|^\s*\|\s*$|^\s*\| \^|^\s*= note: .cargo::unused|^[0-9]+ \| [a-z-]+ = |^warning: unused workspace|^$|^\s+Compiling|^\s+Downloaded|^\s+Downloading"
