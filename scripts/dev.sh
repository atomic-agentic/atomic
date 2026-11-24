#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Run a command in atomic's development container, from the repository root.
# With no command, open an interactive shell.
#
#   scripts/dev.sh                    # a shell with cargo and the test Postgres
#   scripts/dev.sh cargo build
#   scripts/dev.sh cargo test
#
# The container has the pinned Rust toolchain, git, cargo-deny, cosign and bd. It starts the test
# Postgres (scripts/dev-db.sh) and passes its URL as ATOMIC_TEST_DATABASE_URL. Only a container
# engine is needed on the host.
#
# Environment:
#   ATOMIC_CONTAINER      container engine, docker (default) or podman
#   ATOMIC_DEV_MEMORY     memory limit, no swap (default 24g)
#   ATOMIC_DEV_CPUS       CPU limit and cargo job count, a whole number (default 16)
#   ATOMIC_DEV_TIMEOUT    time limit for the command (default 60m)
#   ATOMIC_DEV_CACHE      host directory for cargo's registry and git cache
#                         (default ${XDG_CACHE_HOME:-~/.cache}/atomic/dev)
#   ATOMIC_DEV_DB=off     do not start the test Postgres
#   ATOMIC_DEV_SANDBOX=on let processes in the container create user namespaces and mount
#                         /proc (seccomp and system paths unconfined), so Codex's Linux
#                         sandbox can run, for the sandbox-tests feature
#   ATOMIC_DEV_MOUNTS     colon-separated host directories to mount at the same path, for
#                         checkouts outside this repository (a fork being worked on, say)
#   ATOMIC_DEV_WAIT_LOCK  a file; while it exists, refuse to start (for hosts shared with
#                         benchmarks that must not be disturbed)
#   ATOMIC_DEV_DETACHED=1 let the run outlive the process that started it, as for
#                         `nohup scripts/dev.sh … &`
#   ATOMIC_DEV_MIN_FREE_GB  refuse to start when the disk holding the repository or the cargo
#                         cache has less than this many GiB free (default 10; 0 turns it off)
#   ATOMIC_TEST_*, RUST_LOG, RUST_BACKTRACE are passed through when set.
#
#
# Exit codes: the command's own, or
#   1    the watcher could not be started
#   2    bad usage
#   75   ATOMIC_DEV_WAIT_LOCK exists, or the disk is too full, so nothing was started
#   124  the command hit ATOMIC_DEV_TIMEOUT
#   137  the command was killed, usually by the memory limit
#   130, 143, 129  this script was stopped by SIGINT, SIGTERM or SIGHUP, or its parent died
#                  (143)
#
# Stopping this script, by SIGINT, SIGTERM or SIGHUP, removes the container it started before it
# exits. Killing it outright, or its process group, or the process that started it, removes the
# container within a few seconds: a watcher in a session of its own sees to that. Only that
# container: each run labels its own with a token of its own, and nothing else is touched. The
# test Postgres stays up, since every run on the host shares it. With ATOMIC_DEV_DETACHED=1 the
# run outlives the process that started it (for `nohup scripts/dev.sh … &`); started by the
# system's first process, it always does.
set -euo pipefail

ENGINE="${ATOMIC_CONTAINER:-docker}"
LABEL_KEY="org.atomic-agentic.dev-run"

# Process $1's state and when it started, on one line, or nothing when it is not there or cannot
# be read. On Linux the start is the kernel's count of clock ticks since boot, from field 22 of
# /proc/PID/stat, which stepping the wall clock or changing the time zone leaves as it was; ps's
# start time, a wall-clock time, is read only where there is no /proc, in UTC.
probe() {
    local stat
    if [ -r /proc/self/stat ]; then
        stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 0
        # The command name, in parentheses, may hold spaces; the fields after it do not.
        stat="${stat##*) }"
        # shellcheck disable=SC2086
        set -- ${stat}
        [ -z "${20:-}" ] || echo "$1 ${20}"
        return 0
    fi
    LC_ALL=C TZ=UTC0 ps -o stat= -o lstart= -p "$1" 2>/dev/null |
        sed 's/^ *//; s/ *$//; s/  */ /g' || true
}

# When process $1 started (see probe), or nothing when that cannot be read.
started_at() {
    local seen
    seen="$(probe "$1")"
    [ -z "${seen}" ] || echo "${seen#* }"
}

# Whether process $1 may still be the one that started at $2, so the watcher waits on: there,
# not a zombie, and started then, a process that took its number since being another. When its
# start cannot be read, kill -0 decides, so a failing read does not make a process that is there
# look gone.
running() {
    local seen
    seen="$(probe "$1")"
    if [ -z "${seen}" ]; then
        kill -0 "$1" 2>/dev/null
        return
    fi
    case "${seen}" in Z*) return 1 ;; esac
    [ -z "${2:-}" ] || [ "${seen#* }" = "$2" ]
}

# Whether process $1 is surely the one that started at $2, as signalling it needs: its start read
# and matched. One whose start cannot be read is not signalled. Its identity is checked just before
# each signal; between the check and the signal there is still a moment, with no pidfd, in which
# it could end and its number be taken.
confirmed() {
    local seen
    seen="$(probe "$1")"
    [ -n "${seen}" ] && [ -n "${2:-}" ] || return 1
    case "${seen}" in Z*) return 1 ;; esac
    [ "${seen#* }" = "$2" ]
}

# Remove the containers labelled with token $1, if there are any, and wait a few seconds for
# them to be gone: one the engine is already removing, as `--rm` does when the client is
# stopped, is still listed until it is.
remove_container() {
    local ids _
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        ids="$("${ENGINE}" ps -aq --filter "label=${LABEL_KEY}=$1" 2>/dev/null || true)"
        [ -n "${ids}" ] || return 0
        # shellcheck disable=SC2086
        "${ENGINE}" rm -f ${ids} >/dev/null 2>&1 || true
        sleep 0.5
    done
}

# The watcher, run as `dev.sh --watch-run TOKEN SCRIPT SCRIPT_START PARENT PARENT_START`, PARENT 0
# for none: while the script runs and its parent lives, each the process that started when it
# says, wait; then stop the script, so its trap removes the container and it says how it ended,
# and remove the container itself, again a little later, in case a client was just starting one.
if [ "${1:-}" = "--watch-run" ]; then
    trap '' INT TERM HUP
    token="$2" script="$3" script_start="$4" parent="$5" parent_start="$6"
    while running "${script}" "${script_start}" &&
        { [ "${parent}" = 0 ] || running "${parent}" "${parent_start}"; }; do
        sleep 1
    done
    if confirmed "${script}" "${script_start}"; then
        kill -TERM "${script}" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            running "${script}" "${script_start}" || break
            sleep 1
        done
    fi
    for _ in 1 2 3; do
        remove_container "${token}"
        sleep 2
    done
    exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKERFILE="${ROOT}/dev/Dockerfile"

if command -v sha256sum >/dev/null 2>&1; then
    HASH="$(sha256sum "${DOCKERFILE}" | cut -c1-12)"
else
    HASH="$(shasum -a 256 "${DOCKERFILE}" | cut -c1-12)"
fi
IMAGE="atomic-dev:${HASH}"

MEMORY="${ATOMIC_DEV_MEMORY:-24g}"
CPUS="${ATOMIC_DEV_CPUS:-16}"
LIMIT="${ATOMIC_DEV_TIMEOUT:-60m}"
CACHE="${ATOMIC_DEV_CACHE:-${XDG_CACHE_HOME:-${HOME}/.cache}/atomic/dev}"

case "${CPUS}" in
    '' | *[!0-9]* | 0)
        echo "scripts/dev.sh: ATOMIC_DEV_CPUS must be a whole number above 0, not '${CPUS}'" >&2
        exit 2
        ;;
esac

if [ -n "${ATOMIC_DEV_WAIT_LOCK:-}" ] && [ -e "${ATOMIC_DEV_WAIT_LOCK}" ]; then
    echo "scripts/dev.sh: ${ATOMIC_DEV_WAIT_LOCK} exists, so nothing was started. Try again when it is gone." >&2
    exit 75
fi

# The container is this run's alone, found by a label with a token of this run's: stopping it
# never touches another run's container, or anything else on the host.
TOKEN="$$-$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')"
NAME="atomic-dev-${TOKEN}"

# The watcher starts before anything else, so a parent that dies during setup is seen too. It runs
# in a session of its own and is not this script's child, so what kills this script's process
# group or tree leaves it to remove the container.
# Each process is known by when it started as well as by its number, which another may take.
parent="${PPID}"
if [ "${parent}" -le 1 ] || [ "${ATOMIC_DEV_DETACHED:-0}" = 1 ]; then
    parent=0
fi
watcher_failed() {
    echo "scripts/dev.sh: the watcher that removes this run's container if this script is killed could not be started ($1), so nothing was started" >&2
    exit 1
}
script_start="$(started_at "$$")"
[ -n "${script_start}" ] || watcher_failed "when this script started could not be read"
parent_start=""
if [ "${parent}" != 0 ]; then
    parent_start="$(started_at "${parent}")"
    if [ -z "${parent_start}" ] && ! kill -0 "${parent}" 2>/dev/null &&
        ! ps -p "${parent}" >/dev/null 2>&1; then
        echo "scripts/dev.sh: the process that started this script is gone, so nothing was started" >&2
        exit 1
    fi
    [ -n "${parent_start}" ] || watcher_failed "when the process that started this script started could not be read"
fi
watch=("${BASH}" "${BASH_SOURCE[0]}" --watch-run "${TOKEN}" "$$" "${script_start}" "${parent}" "${parent_start}")
if command -v setsid >/dev/null 2>&1; then
    setsid -f "${watch[@]}" </dev/null >/dev/null 2>&1 || watcher_failed "setsid exited $?"
elif command -v perl >/dev/null 2>&1; then
    perl -MPOSIX -e 'exit 0 if fork; POSIX::setsid(); exec @ARGV' "${watch[@]}" \
        </dev/null >/dev/null 2>&1 || watcher_failed "perl exited $?"
else
    watcher_failed "neither setsid nor perl is installed"
fi

# Stopped by a signal: end the client, giving it a moment, then remove the container, so it is
# gone before this script exits. The client is the one started in the background below, even
# when the signal comes before its number is noted, and not one already waited for, whose number
# another process may have taken; a signal in the moment between the wait's end and the line
# after it can still reach it. A signal before the client starts leaves the container, if one is
# just starting, to the watcher.
client="" waited=0
stopped() {
    trap '' INT TERM HUP
    local pid="${client}"
    if [ -z "${pid}" ] && [ "${waited}" = 0 ]; then
        pid="${!:-}"
    fi
    if [ -n "${pid}" ]; then
        kill "${pid}" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "${pid}" 2>/dev/null || break
            sleep 0.2
        done
    fi
    remove_container "${TOKEN}"
    exit "$1"
}
trap 'stopped 130' INT
trap 'stopped 143' TERM
trap 'stopped 129' HUP

mkdir -p "${CACHE}/cargo"

MIN_FREE_GB="${ATOMIC_DEV_MIN_FREE_GB:-10}"
case "${MIN_FREE_GB}" in
    '' | *[!0-9]*)
        echo "scripts/dev.sh: ATOMIC_DEV_MIN_FREE_GB must be a whole number, not '${MIN_FREE_GB}'" >&2
        exit 2
        ;;
esac
if [ "${MIN_FREE_GB}" -gt 0 ]; then
    for dir in "${ROOT}" "${CACHE}"; do
        free_kb="$(df -Pk "${dir}" | awk 'NR == 2 { print $4 }')"
        if [ "${free_kb}" -lt $((MIN_FREE_GB * 1024 * 1024)) ]; then
            echo "scripts/dev.sh: only $((free_kb / 1024 / 1024)) GiB free on the disk holding ${dir}; ${MIN_FREE_GB} GiB required (ATOMIC_DEV_MIN_FREE_GB). Nothing was started." >&2
            exit 75
        fi
    done
fi

if ! "${ENGINE}" image inspect "${IMAGE}" >/dev/null 2>&1; then
    echo "scripts/dev.sh: building ${IMAGE}" >&2
    "${ENGINE}" build --quiet --tag "${IMAGE}" "$(dirname "${DOCKERFILE}")" >&2
fi

net_flags=()
env_flags=()
if [ "${ATOMIC_DEV_DB:-on}" != "off" ]; then
    "${ROOT}/scripts/dev-db.sh" up
    net_flags=(--network atomic-dev)
    env_flags+=(-e "ATOMIC_TEST_DATABASE_URL=$("${ROOT}/scripts/dev-db.sh" url)")
fi
# The container's /tmp is this run's own, unless /tmp (or /) is mounted in,
# so a test may create and remove entries there that a Codex session on the
# host would otherwise see.
private_tmp=1
if [ -n "${ATOMIC_DEV_MOUNTS:-}" ]; then
    IFS=: read -r -a shared_dirs <<<"${ATOMIC_DEV_MOUNTS}"
    for dir in "${shared_dirs[@]}"; do
        case "${dir}" in / | /tmp | /tmp/) private_tmp=0 ;; esac
    done
fi
if [ "${private_tmp}" = 1 ]; then
    env_flags+=(-e ATOMIC_TEST_PRIVATE_TMP=1)
fi
for name in "${!ATOMIC_TEST_@}" RUST_LOG RUST_BACKTRACE; do
    if [ -n "${!name+x}" ]; then
        env_flags+=(-e "${name}")
    fi
done

mount_flags=()
if [ -n "${ATOMIC_DEV_MOUNTS:-}" ]; then
    IFS=: read -r -a extra_mounts <<<"${ATOMIC_DEV_MOUNTS}"
    for dir in "${extra_mounts[@]}"; do
        [ -n "${dir}" ] || continue
        if [ ! -d "${dir}" ]; then
            echo "scripts/dev.sh: ATOMIC_DEV_MOUNTS names '${dir}', which is not a directory" >&2
            exit 2
        fi
        dir="$(cd "${dir}" && pwd)"
        mount_flags+=(-v "${dir}:${dir}")
    done
fi

tty_flags=()
if [ -t 0 ] && [ -t 1 ]; then
    tty_flags=(-it)
fi

sandbox_flags=()
if [ "${ATOMIC_DEV_SANDBOX:-off}" = "on" ]; then
    # bubblewrap needs unshare(CLONE_NEWUSER), which the engine's default seccomp profile
    # refuses, and mounts a fresh /proc, which the engine's masked /proc paths forbid.
    # Capabilities stay dropped and no-new-privileges stays on.
    sandbox_flags=(--security-opt seccomp=unconfined)
    if [ "${ENGINE}" = "podman" ]; then
        sandbox_flags+=(--security-opt unmask=ALL)
    else
        sandbox_flags+=(--security-opt systempaths=unconfined)
    fi
fi

user_flags=(--user "$(id -u):$(id -g)")
if [ "${ENGINE}" = "podman" ]; then
    user_flags+=(--userns=keep-id)
fi

if [ $# -eq 0 ]; then
    set -- bash
fi


# --memory-swap equal to --memory means no swap. A low --cpu-shares lets other work on the host
# win when they compete for a CPU. timeout runs inside the container, because a timeout on the
# host would only kill the client; --init ends everything left when the command exits. The
# client runs in the background, its standard input this script's, so that a signal reaches
# the traps above while it runs.
"${ENGINE}" run --rm --init ${tty_flags[@]+"${tty_flags[@]}"} \
    --name "${NAME}" \
    --label "${LABEL_KEY}=${TOKEN}" \
    ${net_flags[@]+"${net_flags[@]}"} \
    ${env_flags[@]+"${env_flags[@]}"} \
    "${user_flags[@]}" \
    --memory "${MEMORY}" --memory-swap "${MEMORY}" \
    --cpus "${CPUS}" --cpu-shares 128 \
    --pids-limit 8192 \
    --ulimit core=0 \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --security-opt label=disable \
    ${sandbox_flags[@]+"${sandbox_flags[@]}"} \
    -e HOME=/cache \
    -e CARGO_HOME=/cache/cargo \
    -e "CARGO_BUILD_JOBS=${CPUS}" \
    -v "${CACHE}:/cache" \
    -v "${ROOT}:${ROOT}" \
    ${mount_flags[@]+"${mount_flags[@]}"} \
    -w "${ROOT}" \
    "${IMAGE}" \
    timeout --foreground --kill-after=30s "${LIMIT}" "$@" <&0 &
client=$!
status=0
wait "${client}" || status=$?
# Waited for, so its number is no longer its own: a signal from here on signals no client.
waited=1 client=""
remove_container "${TOKEN}"
exit "${status}"
