#!/usr/bin/env bash
# Host-side health checks for the 1 GB VM, run every 5 minutes by a systemd timer (as root).
# Posts to a Discord webhook (OPS_WEBHOOK_URL) when something needs attention; the same set of
# problems is reported at most every 6 hours. Checks: container state/health/restarts, disk,
# memory, backup age and (once DOMAIN is set) the TLS certificate expiry.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -f /etc/discussion-bot/ops.env ]]; then
    set -a
    # shellcheck source=/dev/null
    . /etc/discussion-bot/ops.env
    set +a
fi

state_dir=/var/lib/discussion-bot
mkdir -p "$state_dir"
problems=() # human-readable details (numbers change between runs)
keys=()     # stable identifiers used to de-duplicate alerts

report() { # report <stable key> <message>
    keys+=("$1")
    problems+=("$2")
}

for service in bot db caddy; do
    id=$(docker compose ps -a -q "$service" 2>/dev/null || true)
    if [[ -z "$id" ]]; then
        [[ "$service" == caddy ]] && continue # added with the web UI
        report "$service:missing" "${service}: container not found (or compose.yaml cannot be read)"
        continue
    fi
    read -r status restarts health < <(docker inspect -f \
        '{{.State.Status}} {{.RestartCount}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$id")
    [[ "$status" == running ]] || report "$service:$status" "${service}: ${status}"
    [[ "$health" == none || "$health" == healthy || "$health" == starting ]] \
        || report "$service:health:$health" "${service}: health ${health}"
    last_file="$state_dir/restarts-$service"
    last=$(cat "$last_file" 2>/dev/null || echo "$restarts")
    ((restarts > last)) && report "$service:restarted" "${service}: restarted $((restarts - last)) time(s)"
    echo "$restarts" > "$last_file"
done

disk=$(df --output=pcent / | tail -n 1 | tr -dc '0-9')
((disk > 85)) && report disk "disk usage ${disk}%"

available_mb=$(awk '/^MemAvailable:/ {print int($2 / 1024)}' /proc/meminfo)
((available_mb < 100)) && report memory "MemAvailable ${available_mb} MB"

newest=$( { find backups/daily backups/pre-deploy -name 'discussion-*.sql.zst.age' -printf '%T@\n' 2>/dev/null || true; } \
    | sort -rn | awk 'NR == 1')
if [[ -z "$newest" ]]; then
    report backup:none "no backup found"
elif (($(date +%s) - ${newest%.*} > 26 * 3600)); then
    report backup:stale "newest backup is older than 26 h"
fi

if [[ -n "${DOMAIN:-}" ]]; then
    end=$(echo | timeout 15 openssl s_client -servername "$DOMAIN" -connect "$DOMAIN:443" 2>/dev/null \
        | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2 || true)
    if [[ -z "$end" ]]; then
        report tls:unreadable "TLS certificate for ${DOMAIN} could not be read"
    elif (($(date -d "$end" +%s) - $(date +%s) < 14 * 86400)); then
        report tls:expiring "TLS certificate for ${DOMAIN} expires ${end}"
    fi
fi

if ((${#problems[@]} == 0)); then
    rm -f "$state_dir/last-alert"
    exit 0
fi

digest=$(printf '%s\n' "${keys[@]}" | sha256sum | cut -d' ' -f1)
if [[ -f "$state_dir/last-alert" ]]; then
    read -r last_digest last_time < "$state_dir/last-alert"
    if [[ "$last_digest" == "$digest" ]] && (($(date +%s) - last_time < 6 * 3600)); then
        exit 0
    fi
fi

message="[discussion-bot $(hostname)] $(printf '%s; ' "${problems[@]}")"
echo "$message" >&2
if [[ -n "${OPS_WEBHOOK_URL:-}" ]]; then
    payload=$(python3 -c 'import json, sys; print(json.dumps({"content": sys.argv[1][:1900], "allowed_mentions": {"parse": []}}))' "$message")
    if ! curl -fsS -m 10 -H 'Content-Type: application/json' -d "$payload" "$OPS_WEBHOOK_URL" >/dev/null; then
        echo "healthwatch.sh: webhook post failed; will retry next run" >&2
        exit 1
    fi
fi
echo "$digest $(date +%s)" > "$state_dir/last-alert"
