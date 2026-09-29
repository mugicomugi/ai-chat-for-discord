#!/usr/bin/env bash
# Stops containers from reaching OCI instance metadata and other link-local services
# (169.254.0.0/16) while keeping DNS, which OCI serves from 169.254.169.254:53.
# Container traffic goes through FORWARD, where the image's own InstanceServices rules
# (OUTPUT only) do not apply. Idempotent; run after docker.service by a systemd unit.
set -euo pipefail

ensure() { # insert the rule at the top of DOCKER-USER unless it already exists
    iptables -C DOCKER-USER "$@" 2>/dev/null || iptables -I DOCKER-USER "$@"
}

# Inserted in reverse order: each -I goes to the top, so the DNS exceptions end up first.
ensure -d 169.254.0.0/16 -p udp -j REJECT
ensure -d 169.254.0.0/16 -p tcp -j REJECT --reject-with tcp-reset
ensure -d 169.254.0.0/16 -p udp --dport 53 -j RETURN
ensure -d 169.254.0.0/16 -p tcp --dport 53 -j RETURN
