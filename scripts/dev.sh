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
#   ATOMIC_DEV_WAIT_LOCK  a file; while it exists, refuse to start (for hosts shared with
#                         benchmarks that must not be disturbed)
#   ATOMIC_TEST_*, RUST_LOG, RUST_BACKTRACE are passed through when set.
#
# Exit codes: the command's own, or
#   2    bad usage
#   75   ATOMIC_DEV_WAIT_LOCK exists, so nothing was started
#   124  the command hit ATOMIC_DEV_TIMEOUT
#   137  the command was killed, usually by the memory limit
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE="${ATOMIC_CONTAINER:-docker}"
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

mkdir -p "${CACHE}/cargo"

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
for name in "${!ATOMIC_TEST_@}" RUST_LOG RUST_BACKTRACE; do
    if [ -n "${!name+x}" ]; then
        env_flags+=(-e "${name}")
    fi
done

tty_flags=()
if [ -t 0 ] && [ -t 1 ]; then
    tty_flags=(-it)
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
# host would only kill the client; --init ends everything left when the command exits.
exec "${ENGINE}" run --rm --init ${tty_flags[@]+"${tty_flags[@]}"} \
    --name "atomic-dev-$$" \
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
    -e HOME=/cache \
    -e CARGO_HOME=/cache/cargo \
    -e "CARGO_BUILD_JOBS=${CPUS}" \
    -v "${CACHE}:/cache" \
    -v "${ROOT}:${ROOT}" \
    -w "${ROOT}" \
    "${IMAGE}" \
    timeout --foreground --kill-after=30s "${LIMIT}" "$@"
