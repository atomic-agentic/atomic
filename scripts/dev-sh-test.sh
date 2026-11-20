#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests that stopping scripts/dev.sh stops and removes the container it
# started: by SIGTERM, SIGINT or SIGHUP, when it is killed outright, and when
# the process that started it dies. Runs on the host, with the container
# engine dev.sh uses, and starts dev.sh with a long sleep each time; nothing
# but the containers those runs start is touched.
#
#   scripts/dev-sh-test.sh
#
# ATOMIC_CONTAINER and ATOMIC_DEV_WAIT_LOCK are honoured as dev.sh honours
# them.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="${ATOMIC_CONTAINER:-docker}"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# The containers a dev.sh run with process id `pid` started, by its label.
containers_of() {
    "${ENGINE}" ps -a --filter "label=org.atomic-agentic.dev-run" --format '{{.ID}} {{.Labels}}' |
        awk -v pid="$1" '{ if (match($0, "org.atomic-agentic.dev-run=" pid "-")) print $1 }'
}

# Wait up to `seconds` for `test` to hold.
within() {
    local seconds="$1"
    shift
    local i
    for i in $(seq 1 $((seconds * 10))); do
        "$@" && return 0
        sleep 0.1
    done
    return 1
}

started() { [ -n "$(containers_of "$1")" ]; }
gone() { [ -z "$(containers_of "$1")" ]; }

# Start dev.sh with a long sleep, under a parent of its own; print the pids of
# both, once its container runs.
start() {
    local log="${WORK}/$1.log"
    # A script's background job starts with SIGINT ignored, which no trap can
    # undo, so dev.sh gets it back at its default, as from a terminal. The
    # parent's own output goes nowhere, so nothing waits on it.
    (
        ATOMIC_DEV_DB=off ATOMIC_DEV_CPUS=1 ATOMIC_DEV_MEMORY=512m \
            env --default-signal=INT "${ROOT}/scripts/dev.sh" sleep 600 </dev/null >"${log}" 2>&1 &
        echo "$!" >"${WORK}/$1.pid"
        wait
    ) </dev/null >/dev/null 2>&1 &
    local parent=$!
    within 10 test -s "${WORK}/$1.pid" || { fail "$1: dev.sh did not start"; return 1; }
    local pid
    pid="$(cat "${WORK}/$1.pid")"
    if ! within 120 started "${pid}"; then
        fail "$1: no container started: $(cat "${log}")"
        kill -KILL "${pid}" 2>/dev/null
        return 1
    fi
    echo "${parent} ${pid}"
}

for how in TERM INT HUP KILL parent; do
    pids="$(start "${how}")" || continue
    read -r parent pid <<<"${pids}"
    case "${how}" in
    parent) kill -KILL "${parent}" ;;
    *) kill "-${how}" "${pid}" ;;
    esac
    if within 10 gone "${pid}"; then
        echo "ok ${how}: the container went with dev.sh" >&2
    else
        fail "${how}: the container outlived dev.sh: $(containers_of "${pid}")"
        # shellcheck disable=SC2046
        "${ENGINE}" rm -f $(containers_of "${pid}") >/dev/null 2>&1
    fi
    kill -KILL "${pid}" "${parent}" 2>/dev/null || true
done

if [ "${failures}" -ne 0 ]; then
    echo "${failures} dev.sh test(s) failed" >&2
    exit 1
fi
echo "dev.sh tests passed" >&2
