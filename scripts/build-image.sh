#!/usr/bin/env bash
# Builds the bot image on the production VM from the checked-out commit: a release build with
# the resource limits of scripts/cargo-vm.sh, then a small runtime image that only copies the
# binary. Prints the image reference to pass to scripts/deploy.sh.
#
#   scripts/build-image.sh          # as the normal user (docker commands use sudo)
set -euo pipefail
cd "$(dirname "$0")/.."

# Everything compiled into the image must be committed (untracked source files included),
# because the tag names the commit.
# static/ and docs/ are compiled in too (the web UI and its /privacy and /terms pages).
if [[ -n "$(git status --porcelain -- src migrations static docs Cargo.toml Cargo.lock Dockerfile .cargo)" ]]; then
    echo "build-image.sh: commit or stash changes under src/, migrations/, static/, docs/ … first; the image tag names a commit" >&2
    exit 1
fi
tag="discord-discussion-bot:git-$(git rev-parse --short=12 HEAD)"

scripts/cargo-vm.sh build --locked --release
context=$(mktemp -d)
trap 'rm -rf -- "$context"' EXIT
cp target/release/discord-discussion-bot "$context/"

sudo docker build --target runtime-prebuilt --build-context prebuilt="$context" \
    --label "org.opencontainers.image.revision=$(git rev-parse HEAD)" -t "$tag" .
echo "build-image.sh: built ${tag}"
echo "Deploy with: sudo scripts/deploy.sh ${tag}"
