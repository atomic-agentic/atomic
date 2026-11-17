#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The test Postgres for atomic's development container.
#
#   scripts/dev-db.sh up       # start it (idempotent) and wait until it accepts connections
#   scripts/dev-db.sh down     # stop and remove it; its data lives in memory and is discarded
#   scripts/dev-db.sh status   # print whether it is running
#   scripts/dev-db.sh url      # print the connection URL used inside the dev network
#
# It runs on a private container network with no published port, so only containers on that
# network (scripts/dev.sh) can reach it. The credentials are fixed and for tests only.
#
# Environment:
#   ATOMIC_CONTAINER   container engine, docker (default) or podman
set -euo pipefail

ENGINE="${ATOMIC_CONTAINER:-docker}"
NETWORK="atomic-dev"
NAME="atomic-dev-postgres"
IMAGE="postgres:18.7-trixie@sha256:2ea8630df176ad090ec92136dee850397c5af8781e4bcf2e4162593691c61bbc"
DB_USER="atomic"
DB_PASSWORD="atomic"
DB_NAME="atomic"
URL="postgres://${DB_USER}:${DB_PASSWORD}@${NAME}:5432/${DB_NAME}"

running() {
    [ "$("${ENGINE}" inspect -f '{{.State.Running}}' "${NAME}" 2>/dev/null)" = "true" ]
}

ensure_network() {
    "${ENGINE}" network inspect "${NETWORK}" >/dev/null 2>&1 \
        || "${ENGINE}" network create "${NETWORK}" >/dev/null
}

up() {
    ensure_network
    if ! running; then
        "${ENGINE}" rm -f "${NAME}" >/dev/null 2>&1 || true
        # Data on tmpfs: tests get a fresh database each time the container starts, and nothing
        # touches the host disk.
        "${ENGINE}" run -d --name "${NAME}" \
            --network "${NETWORK}" \
            --memory 1g --memory-swap 1g --cpus 2 --pids-limit 512 \
            --security-opt no-new-privileges \
            --tmpfs /var/lib/postgresql:rw,size=2g \
            -e POSTGRES_USER="${DB_USER}" \
            -e POSTGRES_PASSWORD="${DB_PASSWORD}" \
            -e POSTGRES_DB="${DB_NAME}" \
            "${IMAGE}" >/dev/null
    fi
    for _ in $(seq 1 60); do
        if "${ENGINE}" exec "${NAME}" pg_isready -q -h 127.0.0.1 -U "${DB_USER}" -d "${DB_NAME}" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "scripts/dev-db.sh: Postgres did not become ready in 60 s" >&2
    "${ENGINE}" logs --tail 20 "${NAME}" >&2 || true
    return 1
}

case "${1:-}" in
    up) up ;;
    down) "${ENGINE}" rm -f "${NAME}" >/dev/null 2>&1 || true ;;
    status) if running; then echo "running: ${NAME} on network ${NETWORK}"; else echo "stopped"; exit 1; fi ;;
    url) echo "${URL}" ;;
    *) echo "usage: scripts/dev-db.sh up|down|status|url" >&2; exit 2 ;;
esac
