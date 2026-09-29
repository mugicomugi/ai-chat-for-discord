#!/usr/bin/env bash
# Deploys a bot image pinned by digest: encrypted pre-deploy backup, pull, restart of the bot
# only, readiness wait. Rolling back is the same command with the previous digest (see
# `sudo tail backups/deploy-history.log`). Check out the commit the image was built from first,
# so compose.yaml and the scripts match the image.
#
#   sudo scripts/deploy.sh ghcr.io/mugicomugi/ai-chat-for-discord@sha256:<digest>
set -euo pipefail
cd "$(dirname "$0")/.."

image="${1:?usage: deploy.sh <image@sha256:digest>}"
if [[ "$image" != *@sha256:* ]]; then
    echo "deploy.sh: pin the image by digest (…@sha256:…), not by tag" >&2
    exit 1
fi
[[ -f .env ]] || { echo "deploy.sh: .env not found" >&2; exit 1; }

previous=$(sed -n 's/^BOT_IMAGE=//p' .env | tail -n 1)
# compose.yaml requires BOT_IMAGE even for `exec db`; the process environment wins over .env.
BOT_IMAGE="${previous:-$image}" scripts/backup.sh pre-deploy

# Pull before touching .env so a bad digest leaves the running configuration unchanged.
docker pull "$image"
if grep -q '^BOT_IMAGE=' .env; then
    sed -i "s|^BOT_IMAGE=.*|BOT_IMAGE=${image}|" .env
else
    printf 'BOT_IMAGE=%s\n' "$image" >> .env
fi

started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ready=0
# --no-deps: a bot deploy must never recreate the database as a side effect (for example the
# one-time 11.4 -> 12.3 upgrade, which has its own procedure in docs/runbook.md).
# --force-recreate: restart even when redeploying the running digest, so readiness is observable.
if docker compose up -d --no-build --no-deps --force-recreate bot; then
    # database_ready (migrations applied) and discord_ready (commands registered) are logged at startup.
    for _ in $(seq 1 90); do
        logs=$(docker compose logs --no-color --since "$started" bot 2>/dev/null || true)
        if grep -q 'database_migration_failed' <<<"$logs"; then
            echo "deploy.sh: migration failed; see docs/runbook.md (マイグレーションが途中で失敗したとき)" >&2
            break
        fi
        if grep -q 'database_ready' <<<"$logs" && grep -q 'discord_ready' <<<"$logs"; then
            ready=1
            break
        fi
        sleep 2
    done
else
    echo "deploy.sh: docker compose up failed" >&2
fi

mkdir -p backups
printf '%s previous=%s deployed=%s ready=%s\n' "$started" "${previous:-none}" "$image" "$ready" \
    >> backups/deploy-history.log

if [[ "$ready" != 1 ]]; then
    echo "deploy.sh: the bot did not become ready." >&2
    echo "Roll back with: sudo scripts/deploy.sh ${previous:-<previous digest>}" >&2
    exit 1
fi

# Keep the three most recent bot images for quick rollback. Images pulled by digest have no tag,
# so --all and --digests are needed to list them.
repository="${image%@*}"
docker image ls --all --digests --format '{{.CreatedAt}}\t{{.Repository}}@{{.Digest}}' "$repository" \
    | sort -r | awk -F '\t' '$2 ~ /@sha256:/ && ++n > 3 {print $2}' \
    | xargs -r docker image rm >/dev/null || echo "deploy.sh: image prune incomplete" >&2
echo "deploy.sh: deployed ${image}"
