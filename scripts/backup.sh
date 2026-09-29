#!/usr/bin/env bash
# Encrypted logical backup of the bot database: mariadb-dump | zstd | age.
# Keeps 7 daily, 4 weekly and 3 pre-deploy copies under backups/, and nothing older than 35 days
# (the retention stated in docs/privacy.md). Optionally copies each file off-host and pings a
# dead man's switch. Run as root (systemd timer or sudo).
#
#   scripts/backup.sh [daily|pre-deploy]
#
# Settings come from /etc/discussion-bot/ops.env (see deploy/ops.env.example):
#   BACKUP_AGE_RECIPIENT  age public key (required); the identity is kept off the VM
#   BACKUP_REMOTE_CMD     optional command run with the backup path appended, e.g. an rclone copy
#   BACKUP_PING_URL       optional URL fetched after a successful backup
set -euo pipefail
umask 077
cd "$(dirname "$0")/.."
if [[ -f /etc/discussion-bot/ops.env ]]; then
    set -a
    # shellcheck source=/dev/null
    . /etc/discussion-bot/ops.env
    set +a
fi

kind="${1:-daily}"
case "$kind" in
    daily | pre-deploy) ;;
    *) echo "usage: backup.sh [daily|pre-deploy]" >&2; exit 2 ;;
esac
: "${BACKUP_AGE_RECIPIENT:?set BACKUP_AGE_RECIPIENT (age public key)}"
for tool in age zstd docker; do
    command -v "$tool" >/dev/null || { echo "backup.sh: $tool is not installed" >&2; exit 1; }
done

mkdir -p backups/daily backups/weekly backups/pre-deploy
file="backups/$kind/discussion-$(date -u +%Y%m%dT%H%M%SZ).sql.zst.age"
trap 'rm -f -- "$file.partial"' EXIT

# pipefail makes a failing mariadb-dump (or docker compose exec) fail the whole backup.
# --hex-blob keeps binary and VECTOR columns intact. Never use SELECT … INTO OUTFILE / LOAD DATA
# for vector tables (MDEV-40853).
docker compose exec -T db sh -c \
    'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" exec mariadb-dump -uroot --single-transaction --hex-blob --routines --events "$MARIADB_DATABASE"' \
    | zstd -q -T1 \
    | age -r "$BACKUP_AGE_RECIPIENT" -o "$file.partial"
mv "$file.partial" "$file"

# The timer fires at 03:30 JST, so the weekly copy is taken on Sundays in JST.
if [[ "$kind" == daily && "$(TZ=Asia/Tokyo date +%u)" == 7 ]]; then
    cp "$file" backups/weekly/
fi

prune() { # keep the newest $2 files in directory $1
    [[ -d "$1" ]] || return 0
    find "$1" -maxdepth 1 -name 'discussion-*.sql.zst.age' -printf '%T@ %p\n' \
        | sort -rn | awk -v keep="$2" 'NR > keep {print $2}' | xargs -r rm -f --
}
prune backups/daily 7
prune backups/weekly 4
prune backups/pre-deploy 3
find backups/daily backups/weekly backups/pre-deploy -maxdepth 1 -name 'discussion-*.sql.zst.age' \
    -mmin +$((35 * 24 * 60)) -delete

if [[ -n "${BACKUP_REMOTE_CMD:-}" ]]; then
    # Word splitting is intended: the command and its arguments come from the root-only env file.
    # shellcheck disable=SC2086
    $BACKUP_REMOTE_CMD "$file"
fi
if [[ -n "${BACKUP_PING_URL:-}" ]]; then
    curl -fsS -m 10 -o /dev/null "$BACKUP_PING_URL" || echo "backup.sh: ping failed" >&2
fi
echo "backup.sh: wrote $file"
