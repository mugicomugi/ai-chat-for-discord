#!/usr/bin/env bash
# Runs cargo on the production VM (1 GB RAM) without starving the live bot and database:
# lowest CPU and I/O priority, and a memory cap so the build swaps instead of pushing MariaDB
# out of RAM. Builds are slow here; run them in the background.
#
#   scripts/cargo-vm.sh clippy --locked --all-targets -- -D warnings
#   scripts/cargo-vm.sh test --locked
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
exec systemd-run --user --scope --quiet \
    -p MemoryHigh="${CARGO_MEMORY_HIGH:-400M}" -p MemoryMax="${CARGO_MEMORY_MAX:-550M}" \
    -- nice -n 19 ionice -c 2 -n 7 cargo "$@"
