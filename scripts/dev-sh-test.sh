#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests that stopping scripts/dev.sh stops and removes the container it
# started, and only that one: by SIGTERM, SIGINT or SIGHUP, with the
# container already gone when dev.sh exits with 143, 130 or 129, also under a
# terminal; when dev.sh is killed outright, or its whole process group is;
# and when the process that started it dies, when dev.sh exits 143, unless
# ATOMIC_DEV_DETACHED=1 lets it outlive that process. A bystander run keeps
# its container through all of them. The watcher knows dev.sh and its parent
# by when they started, so a process that took one's number is never
# signalled; a ps that fails does not make dev.sh look gone; a watcher that
# cannot start is reported; and a signal after the client was waited for
# signals no client.
#
# Runs on the host, with the container engine dev.sh uses (docker or podman),
# and starts dev.sh with a long sleep each time. Every run's engine is a shim
# that labels the containers it makes with this test's own token, and lists
# and removes only those, so nothing but the containers this test starts is
# ever touched, even when dev.sh's own label match is wrong.
#
#   scripts/dev-sh-test.sh
#
# ATOMIC_CONTAINER and ATOMIC_DEV_WAIT_LOCK are honoured as dev.sh honours
# them: with the lock there, nothing starts and the test exits 75. Linux only
# for the cases that need to outlive dev.sh's parent (they use a subreaper).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="${ATOMIC_CONTAINER:-docker}"
KEY="org.atomic-agentic.dev-run"
TEST_KEY="org.atomic-agentic.dev-sh-test"
if [ -n "${ATOMIC_DEV_WAIT_LOCK:-}" ] && [ -e "${ATOMIC_DEV_WAIT_LOCK}" ]; then
    echo "scripts/dev-sh-test.sh: ${ATOMIC_DEV_WAIT_LOCK} exists, so nothing was started" >&2
    exit 75
fi
REAL="$(command -v "${ENGINE}")" || {
    echo "scripts/dev-sh-test.sh: ${ENGINE} is not installed" >&2
    exit 1
}
TAG="$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')"
WORK="$(mktemp -d)" || exit 1
failures=0

# This test's containers alone: those with its own label.
ours_only() { "${REAL}" ps -aq --filter "label=${TEST_KEY}=${TAG}" "$@" 2>/dev/null; }
cleanup() {
    local left
    left="$(ours_only)"
    # shellcheck disable=SC2086
    [ -z "${left}" ] || "${REAL}" rm -f ${left} >/dev/null 2>&1
    rm -rf "${WORK}"
}
trap cleanup EXIT

# The engine dev.sh runs under its own name, first on PATH: it adds this
# test's label to each container it makes, lists only those, and removes
# only those, so a label match loosened in dev.sh reaches no one else's.
mkdir -p "${WORK}/engine"
{
    echo '#!/usr/bin/env bash'
    printf 'real=%q key=%q tag=%q\n' "${REAL}" "${TEST_KEY}" "${TAG}"
    cat <<'SHIM'
if [ -n "${ATOMIC_DEV_TEST_SLOW_PS:-}" ] && [ "${1:-}" = ps ]; then
    sleep "${ATOMIC_DEV_TEST_SLOW_PS}"
fi
case "${1:-}" in
run)
    shift
    exec "${real}" run --label "${key}=${tag}" "$@"
    ;;
ps)
    shift
    exec "${real}" ps --filter "label=${key}=${tag}" "$@"
    ;;
rm)
    shift
    flags=() ids=()
    for arg in "$@"; do
        case "${arg}" in -*) flags+=("${arg}") ;; *) ids+=("${arg}") ;; esac
    done
    mine=()
    for id in ${ids[@]+"${ids[@]}"}; do
        label="$("${real}" inspect --format "{{index .Config.Labels \"${key}\"}}" "${id}" 2>/dev/null)"
        [ "${label}" = "${tag}" ] && mine+=("${id}")
    done
    [ "${#mine[@]}" -eq 0 ] || exec "${real}" rm ${flags[@]+"${flags[@]}"} "${mine[@]}"
    exit 0
    ;;
*) exec "${real}" "$@" ;;
esac
SHIM
} >"${WORK}/engine/${ENGINE##*/}"
chmod +x "${WORK}/engine/${ENGINE##*/}"
export PATH="${WORK}/engine:${PATH}" ATOMIC_CONTAINER="${ENGINE##*/}"

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# The containers a dev.sh run with process id `pid` started: this test's,
# whose run label is exactly its token, the pid and sixteen hex digits.
containers_of() {
    local id
    for id in $(ours_only --filter "label=${KEY}"); do
        "${REAL}" inspect --format "{{index .Config.Labels \"${KEY}\"}}" "${id}" 2>/dev/null |
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

# When process $1 started, or nothing when it is not there.
started_at() { ps -o lstart= -p "$1" 2>/dev/null | sed 's/^ *//; s/ *$//'; }
running() { kill -0 "$1" 2>/dev/null && ! ps -o stat= -p "$1" | grep -q '^Z'; }
# Whether process $1 is still the one that started at $2: never one that
# took its number after it was waited for.
same() { [ -n "$2" ] && running "$1" && [ "$(started_at "$1")" = "$2" ]; }
# Kill process $1 if it is still the one that started at $2.
kill_same() { ! same "$1" "$2" || kill -KILL "$1" 2>/dev/null; }
started() { [ -n "$(containers_of "$1")" ] || ! running "$1"; }
gone() { [ -z "$(containers_of "$1")" ]; }

# A subreaper for the runs whose parent the test kills: it starts the parent,
# which starts dev.sh, and once the parent is gone waits for dev.sh, so its
# exit status is still known.
cat >"${WORK}/reaper.py" <<'PY'
import ctypes, os, signal, sys
work, how, cmd = sys.argv[1], sys.argv[2], sys.argv[3:]
if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
    sys.exit("cannot become a subreaper")
def note(name, value):
    with open(f"{work}/{how}.{name}.new", "w") as f:
        f.write(str(value))
    os.rename(f"{work}/{how}.{name}.new", f"{work}/{how}.{name}")
parent = os.fork()
if parent == 0:
    devsh = os.fork()
    if devsh == 0:
        log = os.open(f"{work}/{how}.log", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.dup2(os.open("/dev/null", os.O_RDONLY), 0)
        os.dup2(log, 1)
        os.dup2(log, 2)
        os.execvp(cmd[0], cmd)
    note("pid", devsh)
    while True:
        signal.pause()
note("parent", parent)
while True:
    try:
        pid, status = os.wait()
    except ChildProcessError:
        break
    try:
        devsh = int(open(f"{work}/{how}.pid").read())
    except (OSError, ValueError):
        devsh = None
    if pid == devsh:
        note("status", os.waitstatus_to_exitcode(status))
PY

# Start dev.sh with a long sleep, as `how` needs: under a parent of its own,
# which notes dev.sh's pid and later its exit status; in a session of its
# own for a group kill; under a terminal; or under a subreaper, for a parent
# the test kills. Prints the parent's pid and dev.sh's, and when each
# started, once its container runs.
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
    parent | detached)
        [ "${how}" = parent ] || devsh=(env ATOMIC_DEV_DETACHED=1 "${devsh[@]}")
        python3 "${WORK}/reaper.py" "${WORK}" "${how}" "${devsh[@]}" </dev/null >/dev/null 2>&1 &
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
    case "${how}" in
    parent | detached)
        within 10 test -s "${WORK}/${how}.parent" || { fail "${how}: no parent"; return 1; }
        parent="$(cat "${WORK}/${how}.parent")"
        ;;
    esac
    local pid parent_start pid_start
    pid="$(cat "${WORK}/${how}.pid")"
    parent_start="$(started_at "${parent}")"
    pid_start="$(started_at "${pid}")"
    within 120 started "${pid}"
    if ! running "${pid}"; then
        fail "${how}: dev.sh ended before its container ran: $(cat "${log}")"
        return 1
    fi
    if [ -z "$(containers_of "${pid}")" ]; then
        fail "${how}: no container started: $(cat "${log}")"
        kill_same "${pid}" "${pid_start}"
        return 1
    fi
    echo "${parent} ${pid} ${parent_start// /_} ${pid_start// /_}"
}

# The bystander: another run, as another lane's on a shared host, whose
# container each stop below must leave alone.
by="$(start bystander)" || {
    fail "bystander: did not start"
    exit 1
}
read -r _ by_pid _ by_start <<<"${by}"
by_start="${by_start//_/ }"
bystander_kept() {
    if ! same "${by_pid}" "${by_start}" || [ -z "$(containers_of "${by_pid}")" ]; then
        fail "$1: the bystander's container was removed with this run's"
    fi
}

for how in TERM INT HUP pty KILL group parent detached; do
    pids="$(start "${how}")" || {
        fail "${how}: did not start"
        continue
    }
    read -r parent pid parent_start pid_start <<<"${pids}"
    parent_start="${parent_start//_/ }" pid_start="${pid_start//_/ }"
    case "${how}" in
    pty) kill -TERM "${pid}" ;;
    parent | detached) kill -KILL "${parent}" ;;
    group) kill -KILL -- "-${pid}" ;;
    *) kill "-${how}" "${pid}" ;;
    esac
    if [ "${how}" = detached ]; then
        # Its parent gone, it runs on; stopped, it goes as any run does.
        sleep 5
        if same "${pid}" "${pid_start}" && [ -n "$(containers_of "${pid}")" ]; then
            echo "ok detached: dev.sh and its container outlived its parent" >&2
        else
            fail "detached: dev.sh did not outlive its parent: $(cat "${WORK}/${how}.log")"
        fi
        kill -TERM "${pid}"
    fi
    case "${how}" in
    TERM | INT | HUP | pty | parent | detached)
        # The trap removes the container before dev.sh exits; when the
        # parent died, the watcher stops dev.sh first.
        within 20 test -s "${WORK}/${how}.status"
        left="$(containers_of "${pid}")"
        status="$(cat "${WORK}/${how}.status" 2>/dev/null)"
        case "${how}" in
        INT) want=130 ;;
        HUP) want=129 ;;
        *) want=143 ;;
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
    bystander_kept "${how}"
    # Whatever is left of this run: its own container, and its processes if
    # they are still the ones it started, never ones that took their numbers.
    left="$(containers_of "${pid}")"
    # shellcheck disable=SC2086
    [ -z "${left}" ] || "${REAL}" rm -f ${left} >/dev/null 2>&1
    [ -s "${WORK}/${how}.status" ] || kill_same "${pid}" "${pid_start}"
    kill_same "${parent}" "${parent_start}"
done

# A signal after the client was waited for: the trap signals no client,
# whose number another process may have taken. The client's command ends at
# once, and the engine's listing is slowed, so the signal comes while dev.sh
# removes its container after waiting.
(
    env ATOMIC_DEV_DB=off ATOMIC_DEV_CPUS=1 ATOMIC_DEV_MEMORY=512m ATOMIC_DEV_TEST_SLOW_PS=4 \
        bash -x "${ROOT}/scripts/dev.sh" true </dev/null >"${WORK}/late.log" 2>&1 &
    echo "$!" >"${WORK}/late.pid"
    wait "$!"
    echo "$?" >"${WORK}/late.status"
) </dev/null >/dev/null 2>&1 &
if within 10 test -s "${WORK}/late.pid" &&
    within 120 grep -q '^+ remove_container ' "${WORK}/late.log"; then
    kill -TERM "$(cat "${WORK}/late.pid")"
    within 30 test -s "${WORK}/late.status"
    status="$(cat "${WORK}/late.status" 2>/dev/null)"
    signalled="$(grep -E '^\++ kill ' "${WORK}/late.log")"
    if [ "${status}" = 143 ] && [ -z "${signalled}" ]; then
        echo "ok late: a signal after the client was waited for signalled no client" >&2
    else
        fail "late: dev.sh exited '${status}' (want 143), signalling '${signalled}'"
    fi
else
    fail "late: dev.sh did not reach its wait: $(cat "${WORK}/late.log")"
fi
bystander_kept late

# The watcher run by hand, with a sleep in dev.sh's place. Its token names
# no container, and the engine is the shim.
sleep 300 </dev/null >/dev/null 2>&1 &
other=$!
other_start="$(started_at "${other}")"
watch() { "${BASH}" "${ROOT}/scripts/dev.sh" --watch-run "${TAG}-none" "$@" </dev/null >/dev/null 2>&1 & }

# A process that took dev.sh's number, after dev.sh's parent went, is not
# dev.sh: it is not signalled, and the watcher ends.
watch "${other}" "Thu Jan  1 00:00:00 1970" "$$" "Thu Jan  1 00:00:00 1970"
watcher=$!
if within 15 eval '! running '"${watcher}" && same "${other}" "${other_start}"; then
    echo "ok reused: the watcher left alone a process that took dev.sh's number" >&2
else
    fail "reused: the watcher signalled, or waited on, a process that took dev.sh's number"
    kill -KILL "${watcher}" 2>/dev/null
fi
wait "${watcher}" 2>/dev/null

# A ps that fails does not make dev.sh look gone: the watcher waits on until
# dev.sh is.
mkdir -p "${WORK}/no-ps"
printf '#!/bin/sh\nexit 1\n' >"${WORK}/no-ps/ps"
chmod +x "${WORK}/no-ps/ps"
PATH="${WORK}/no-ps:${PATH}" watch "${other}" "${other_start}" 0 ""
watcher=$!
sleep 4
if running "${watcher}" && same "${other}" "${other_start}"; then
    kill -KILL "${other}"
    wait "${other}" 2>/dev/null
    if within 15 eval '! running '"${watcher}"; then
        echo "ok no-ps: a failing ps did not make dev.sh look gone" >&2
    else
        fail "no-ps: the watcher outlived dev.sh"
    fi
else
    fail "no-ps: the watcher took a failing ps for dev.sh gone"
fi
kill -KILL "${watcher}" 2>/dev/null
wait "${watcher}" 2>/dev/null
kill_same "${other}" "${other_start}"

# A watcher that cannot start is reported, and nothing starts: setsid fails,
# or ps cannot say when dev.sh, or the process that started it, started.
mkdir -p "${WORK}/no-setsid" "${WORK}/ps-fails"
printf '#!/bin/sh\nexit 1\n' >"${WORK}/no-setsid/setsid"
{
    echo '#!/usr/bin/env bash'
    printf 'real=%q\n' "$(command -v ps)"
    echo 'for arg; do [ "${arg}" != "${ATOMIC_DEV_TEST_PS_FAILS}" ] || exit 1; done'
    echo 'exec "${real}" "$@"'
} >"${WORK}/ps-fails/ps"
chmod +x "${WORK}/no-setsid/setsid" "${WORK}/ps-fails/ps"
for case in no-setsid ps-self ps-parent; do
    case "${case}" in
    no-setsid) run=(env PATH="${WORK}/no-setsid:${PATH}" "${ROOT}/scripts/dev.sh" sleep 600) ;;
    # dev.sh takes the shell's process id, and then the shell is its parent.
    ps-self) run=(env PATH="${WORK}/ps-fails:${PATH}" bash -c \
        'export ATOMIC_DEV_TEST_PS_FAILS=$$; exec "$@"' _ "${ROOT}/scripts/dev.sh" sleep 600) ;;
    ps-parent) run=(env PATH="${WORK}/ps-fails:${PATH}" bash -c \
        'export ATOMIC_DEV_TEST_PS_FAILS=$$; "$@"' _ "${ROOT}/scripts/dev.sh" sleep 600) ;;
    esac
    out="$(ATOMIC_DEV_DB=off "${run[@]}" </dev/null 2>&1)"
    status=$?
    if [ "${status}" = 1 ] &&
        grep -q "the watcher that removes this run's container .* could not be started" <<<"${out}"; then
        echo "ok ${case}: a watcher that could not start was reported" >&2
    else
        fail "${case}: dev.sh exited ${status}, saying '${out}'"
    fi
done
bystander_kept watcher

# The bystander, stopped last, goes as any run does.
kill -TERM "${by_pid}"
within 20 test -s "${WORK}/bystander.status"
status="$(cat "${WORK}/bystander.status" 2>/dev/null)"
if [ "${status}" = 143 ] && within 10 gone "${by_pid}"; then
    echo "ok bystander: its container lasted until it was stopped" >&2
else
    fail "bystander: dev.sh exited '${status}' leaving '$(containers_of "${by_pid}")'"
fi

if [ "${failures}" -ne 0 ]; then
    echo "${failures} dev.sh test(s) failed" >&2
    exit 1
fi
echo "dev.sh tests passed" >&2
