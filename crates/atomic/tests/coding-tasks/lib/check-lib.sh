# SPDX-License-Identifier: Apache-2.0
#
# Sourced by each task's check.sh, which is run as `check.sh WORKSPACE`.
#
# It grades a copy of the workspace, never the workspace itself: G is a fresh
# copy without .git, with the task's hidden/ files laid over it. The copy
# refuses a link, a special file, a Python startup hook, a top-level name that
# a hidden file uses (`hidden_*`) and a top-level module that shares its name
# with Python's standard library, so the attempt's files cannot stand in for
# the hidden tests or the test runner. The tests run in a clean environment,
# and the test runner itself in Python's isolated mode.
#
# Grading runs the attempt's code, with the privileges of whoever runs the
# check: it is not a sandbox, and an attempt that sets out to game it can (a
# test process that exits 0 early passes). Run checks inside a sandbox, on
# attempts you would run anyway.
#
# Exit codes of check.sh: 0 when the task is done, 1 when it is not, 2 for bad
# usage.
set -euo pipefail

# A clean environment: nothing from the caller's reaches the attempt's code
# but where temporary files go.
if [ -z "${ATOMIC_TASK_CLEAN:-}" ]; then
    exec env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/nonexistent LC_ALL=C.UTF-8 \
        TMPDIR="${TMPDIR:-/tmp}" ATOMIC_TASK_CLEAN=1 bash "$0" "$@"
fi

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

G="$(mktemp -d "${TMPDIR}/atomic-task-grade.XXXXXX")"
trap 'rm -rf "${G}"' EXIT
# Every .git, at any depth, stays behind; a copy that cannot be read whole
# fails the check.
(cd "${WS}" && tar --exclude=.git -cf - .) | (cd "${G}" && tar -xf -) \
    || fail "the workspace could not be copied whole"
find "${G}" -name __pycache__ -prune -exec rm -rf {} +
# What was copied, not what the workspace held a moment before.
odd="$(find "${G}" -name .venv -prune -o ! -type f ! -type d -print -quit)"
[ -z "${odd}" ] || fail "the workspace holds a link or a special file: ${odd#"${G}"/}"
rm -rf "${G}/.venv"
hooked="$(find "${G}" \( -name sitecustomize.py -o -name usercustomize.py \
    -o -name conftest.py -o -name '*.pth' \) -print -quit)"
[ -z "${hooked}" ] || fail "the workspace holds a Python startup hook: ${hooked#"${G}"/}"
reserved="$(find "${G}" -mindepth 1 -maxdepth 1 -name 'hidden_*' -print -quit)"
[ -z "${reserved}" ] || fail "the workspace holds ${reserved#"${G}"/}, a name the hidden tests use"
shadow="$(cd "${G}" && python3 -I -c '
import os, sys
names = set(sys.stdlib_module_names)
for entry in sorted(os.listdir(".")):
    module = entry.split(".")[0] if os.path.isfile(entry) else entry
    if module in names and (os.path.isdir(entry) or entry.endswith((".py", ".pyc", ".so"))):
        print(entry)
        break
')"
[ -z "${shadow}" ] || fail "the workspace holds ${shadow}, which shares its name with a standard library module"
if [ -d "${TASK_DIR}/hidden" ]; then
    cp -rf "${TASK_DIR}/hidden/." "${G}/"
fi
cd "${G}"

export PYTHONDONTWRITEBYTECODE=1 PYTHONNOUSERSITE=1

# py_test <module or discover arguments...>: unittest in the current
# directory, isolated from the environment and from user site-packages, with
# the directory after the standard library on its path, and with a limit.
py_test() {
    timeout -k 5 60 python3 -I -B -c '
import os, sys
sys.path.append(os.getcwd())
import unittest
unittest.main(module=None, argv=["python3 -m unittest"] + sys.argv[1:])
' "$@" >&2
}

# hidden_tests: the task's hidden/hidden_test.py.
hidden_tests() {
    py_test hidden_test || fail "the hidden tests fail"
}

# unchanged <path relative to the seed>: the seed's file, or every file the
# seed has under a directory, is as the seed has it. Files the attempt adds
# beside them are its own.
unchanged() {
    if [ -d "${SEED}/$1" ]; then
        local file
        while IFS= read -r -d '' file; do
            file="${file#./}"
            cmp -s "${SEED}/$1/${file}" "${G}/$1/${file}" || fail "$1/${file} was changed"
        done < <(cd "${SEED}/$1" && find . -type f -not -path '*/__pycache__/*' -print0)
    else
        cmp -s "${SEED}/$1" "${G}/$1" || fail "$1 was changed"
    fi
}
