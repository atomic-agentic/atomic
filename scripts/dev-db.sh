#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The test Postgres for atomic's development container.
#
#   scripts/dev-db.sh up       # start it (idempotent) and wait until it accepts connections
#   scripts/dev-db.sh down     # stop and remove it; its data lives in memory and is discarded
#   scripts/dev-db.sh status   # print whether it is running
#   scripts/dev-db.sh url      # print the connection URL used inside the dev network
#   scripts/dev-db.sh usage    # print how full its in-memory disk is, its size and its schemas
#
# It runs on a private container network with no published port, so only containers on that
# network (scripts/dev.sh) can reach it. The credentials are fixed and for tests only.
#
# Environment:
#   ATOMIC_CONTAINER   container engine, docker (default) or podman
#
# Each test makes a schema of its own and drops it afterwards; a killed test
# process leaves its schemas behind, and the next test process sweeps those
# older than an hour (atomic_durable::testing). `up` warns when the disk is
# more than 80% full anyway.
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
            local used
            used="$(used_percent 2>/dev/null || echo 0)"
            if [ "${used:-0}" -gt 80 ]; then
                echo "scripts/dev-db.sh: the test database's disk is ${used}% full; see scripts/dev-db.sh usage" >&2
            fi
            return 0
        fi
        sleep 1
    done
    echo "scripts/dev-db.sh: Postgres did not become ready in 60 s" >&2
    "${ENGINE}" logs --tail 20 "${NAME}" >&2 || true
    return 1
}

psql() {
    "${ENGINE}" exec "${NAME}" psql -q -At -U "${DB_USER}" -d "${DB_NAME}" -c "$1"
}

# The data disk's use, as a percentage without the sign.
used_percent() {
    "${ENGINE}" exec "${NAME}" df -P /var/lib/postgresql | awk 'NR == 2 { sub("%", "", $5); print $5 }'
}

usage() {
    running || { echo "stopped"; return 1; }
    local disk schemas size
    disk="$("${ENGINE}" exec "${NAME}" df -hP /var/lib/postgresql | awk 'NR == 2 { print $3 " of " $2 " (" $5 ")" }')"
    size="$(psql "SELECT pg_size_pretty(pg_database_size(current_database()))")"
    schemas="$(psql "SELECT count(*) FROM pg_namespace WHERE nspname NOT LIKE 'pg\_%' AND nspname NOT IN ('information_schema', 'public')")"
    echo "disk: ${disk} used"
    echo "database: ${size}"
    echo "schemas: ${schemas} besides public and the system's"
    echo "test schemas by prefix:"
    psql "SELECT count(*), regexp_replace(nspname, '_[0-9].*', '') AS prefix FROM pg_namespace WHERE nspname NOT LIKE 'pg\_%' AND nspname NOT IN ('information_schema', 'public') GROUP BY prefix ORDER BY 1 DESC" |
        awk -F'|' '{ printf "  %6d %s\n", $1, $2 }'
}

case "${1:-}" in
    up) up ;;
    down) "${ENGINE}" rm -f "${NAME}" >/dev/null 2>&1 || true ;;
    status) if running; then echo "running: ${NAME} on network ${NETWORK}"; else echo "stopped"; exit 1; fi ;;
    url) echo "${URL}" ;;
    usage) usage ;;
    *) echo "usage: scripts/dev-db.sh up|down|status|url|usage" >&2; exit 2 ;;
esac
