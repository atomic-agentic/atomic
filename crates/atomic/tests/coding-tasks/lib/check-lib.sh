# SPDX-License-Identifier: Apache-2.0
#
# Sourced by each task's check.sh, which is run as `check.sh WORKSPACE`.
#
# It grades a copy, never the workspace itself: G is a fresh copy of the
# workspace without .git, with the task's hidden/ files laid over it, so the
# agent never sees the hidden tests and cannot replace them. A workspace holding
# a link, a special file or a Python startup hook fails, so nothing reaches
# outside G.
#
# The checks grade honest attempts. They are not hardened against an agent that
# sets out to game them.
#
# Exit codes of check.sh: 0 when the task is done, 1 when it is not, 2 for bad
# usage.
set -euo pipefail

TASK_DIR="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
SEED="${TASK_DIR}/seed"

fail() {
    echo "check: $*" >&2
    exit 1
}

if [ $# -ne 1 ] || [ ! -d "$1" ]; then
    echo "usage: check.sh WORKSPACE" >&2
    exit 2
fi
WS="$(cd "$1" && pwd)"

odd="$(find "${WS}" -path "${WS}/.git" -prune -o ! -type f ! -type d -print -quit)"
[ -z "${odd}" ] || fail "the workspace holds a link or a special file: ${odd#"${WS}"/}"
hooked="$(find "${WS}" -path "${WS}/.git" -prune -o \( -name sitecustomize.py -o -name usercustomize.py \
    -o -name conftest.py -o -name '*.pth' \) -print -quit)"
[ -z "${hooked}" ] || fail "the workspace holds a Python startup hook: ${hooked#"${WS}"/}"

G="$(mktemp -d "${TMPDIR:-/tmp}/atomic-task-grade.XXXXXX")"
trap 'rm -rf "${G}"' EXIT
(cd "${WS}" && tar --exclude=./.git -cf - .) | (cd "${G}" && tar -xf -)
find "${G}" -name __pycache__ -prune -exec rm -rf {} +
if [ -d "${TASK_DIR}/hidden" ]; then
    cp -rf "${TASK_DIR}/hidden/." "${G}/"
fi
cd "${G}"

export PYTHONDONTWRITEBYTECODE=1
unset PYTHONPATH PYTHONHOME PYTHONSTARTUP PYTHONSAFEPATH

# py_test <module or discover arguments...>: unittest in G, with a limit.
py_test() {
    timeout -k 5 60 python3 -B -m unittest "$@" >&2
}

# hidden_tests: the task's hidden/hidden_test.py.
hidden_tests() {
    py_test hidden_test || fail "the hidden tests fail"
}

# unchanged <path relative to the seed>: a file or directory is as the seed has it.
unchanged() {
    if [ -d "${SEED}/$1" ]; then
        diff -r -x __pycache__ "${SEED}/$1" "${G}/$1" >&2 || fail "$1 was changed"
    else
        cmp -s "${SEED}/$1" "${G}/$1" || fail "$1 was changed"
    fi
}
