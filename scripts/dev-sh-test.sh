#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests that stopping scripts/dev.sh stops and removes the container it
# started: by SIGTERM, SIGINT or SIGHUP, with the container already gone when
# dev.sh exits with 143, 130 or 129, also under a terminal; when dev.sh is
# killed outright, or its whole process group is; and when the process that
# started it dies. Runs on the host, with the container engine dev.sh uses
# (docker or podman), and starts dev.sh with a long sleep each time; nothing
# but the containers those runs start is touched.
#
#   scripts/dev-sh-test.sh
#
# ATOMIC_CONTAINER and ATOMIC_DEV_WAIT_LOCK are honoured as dev.sh honours
# them: with the lock there, nothing starts and the test exits 75.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="${ATOMIC_CONTAINER:-docker}"
KEY="org.atomic-agentic.dev-run"
if [ -n "${ATOMIC_DEV_WAIT_LOCK:-}" ] && [ -e "${ATOMIC_DEV_WAIT_LOCK}" ]; then
    echo "scripts/dev-sh-test.sh: ${ATOMIC_DEV_WAIT_LOCK} exists, so nothing was started" >&2
    exit 75
fi
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# The containers a dev.sh run with process id `pid` started: those whose
# label is exactly its token, the pid and sixteen hex digits.
containers_of() {
    local id
    for id in $("${ENGINE}" ps -aq --filter "label=${KEY}" 2>/dev/null); do
        "${ENGINE}" inspect --format "{{index .Config.Labels \"${KEY}\"}}" "${id}" 2>/dev/null |
            grep -qE "^$1-[0-9a-f]{16}\$" && echo "${id}"
    done
}

# Wait up to `seconds` for the command to hold.
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

running() { kill -0 "$1" 2>/dev/null && ! ps -o stat= -p "$1" | grep -q '^Z'; }
started() { [ -n "$(containers_of "$1")" ] || ! running "$1"; }
gone() { [ -z "$(containers_of "$1")" ]; }

# Start dev.sh with a long sleep, as `how` needs: under a parent of its own,
# which notes dev.sh's pid and later its exit status, in a session of its
# own for a group kill, or under a terminal. Prints the parent's pid and
# dev.sh's, once its container runs.
start() {
    local how="$1" log="${WORK}/$1.log"
    local devsh=(env --default-signal=INT ATOMIC_DEV_DB=off ATOMIC_DEV_CPUS=1
        ATOMIC_DEV_MEMORY=512m "${ROOT}/scripts/dev.sh" sleep 600)
    case "${how}" in
    group)
        (
            setsid "${devsh[@]}" </dev/null >"${log}" 2>&1 &
            echo "$!" >"${WORK}/${how}.pid"
            wait "$!"
            echo "$?" >"${WORK}/${how}.status"
        ) </dev/null >/dev/null 2>&1 &
        ;;
    pty)
        (
            script -qfec "exec ${devsh[*]}" /dev/null </dev/null >"${log}" 2>&1 &
            s=$!
            for _ in $(seq 1 50); do
                ps -o pid= --ppid "${s}" | tr -d ' ' | head -1 >"${WORK}/${how}.pid"
                [ -s "${WORK}/${how}.pid" ] && break
                sleep 0.1
            done
            wait "${s}"
            echo "$?" >"${WORK}/${how}.status"
        ) </dev/null >/dev/null 2>&1 &
        ;;
    *)
        (
            "${devsh[@]}" </dev/null >"${log}" 2>&1 &
            echo "$!" >"${WORK}/${how}.pid"
            wait "$!"
            echo "$?" >"${WORK}/${how}.status"
        ) </dev/null >/dev/null 2>&1 &
        ;;
    esac
    local parent=$!
    within 10 test -s "${WORK}/${how}.pid" || { fail "${how}: dev.sh did not start"; return 1; }
    local pid
    pid="$(cat "${WORK}/${how}.pid")"
    within 120 started "${pid}"
    if ! running "${pid}"; then
        fail "${how}: dev.sh ended before its container ran: $(cat "${log}")"
        return 1
    fi
    if [ -z "$(containers_of "${pid}")" ]; then
        fail "${how}: no container started: $(cat "${log}")"
        kill -KILL "${pid}" 2>/dev/null
        return 1
    fi
    echo "${parent} ${pid}"
}

for how in TERM INT HUP pty KILL group parent; do
    pids="$(start "${how}")" || continue
    read -r parent pid <<<"${pids}"
    case "${how}" in
    pty) kill -TERM "${pid}" ;;
    parent) kill -KILL "${parent}" ;;
    group) kill -KILL -- "-${pid}" ;;
    *) kill "-${how}" "${pid}" ;;
    esac
    case "${how}" in
    TERM | INT | HUP | pty)
        # The trap removes the container before dev.sh exits.
        within 15 test -s "${WORK}/${how}.status"
        left="$(containers_of "${pid}")"
        status="$(cat "${WORK}/${how}.status" 2>/dev/null)"
        case "${how}" in
        TERM | pty) want=143 ;;
        INT) want=130 ;;
        HUP) want=129 ;;
        esac
        if [ "${status}" = "${want}" ] && [ -z "${left}" ]; then
            echo "ok ${how}: dev.sh exited ${status} with its container gone" >&2
        else
            fail "${how}: dev.sh exited '${status}' (want ${want}) leaving '${left}'"
        fi
        ;;
    *)
        if within 15 gone "${pid}"; then
            echo "ok ${how}: the container went with dev.sh" >&2
        else
            fail "${how}: the container outlived dev.sh: $(containers_of "${pid}")"
        fi
        ;;
    esac
    # Whatever is left of this run: its own container only.
    left="$(containers_of "${pid}")"
    # shellcheck disable=SC2086
    [ -z "${left}" ] || "${ENGINE}" rm -f ${left} >/dev/null 2>&1
    kill -KILL "${pid}" "${parent}" 2>/dev/null || true
done

if [ "${failures}" -ne 0 ]; then
    echo "${failures} dev.sh test(s) failed" >&2
    exit 1
fi
echo "dev.sh tests passed" >&2
