# Helpers for tests/run.sh: a scratch directory, a sample tree, and checks that count failures.

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/backup-tests.XXXXXX") || exit 1
trap 'rm -rf "$SCRATCH"' EXIT
FAILED=0
PASSED=0

ok() {
    PASSED=$((PASSED + 1))
    echo "ok    $1"
}

not_ok() {
    FAILED=$((FAILED + 1))
    echo "FAIL  $1" >&2
}

# check NAME EXPECTED ACTUAL
check() {
    if [ "$2" = "$3" ]; then
        ok "$1"
    else
        not_ok "$1"
        printf '      expected: %s\n      got:      %s\n' "$2" "$3" >&2
    fi
}

# sample DIR: a small tree with spaces, nesting and dot files.
sample() {
    mkdir -p "$1/docs/old drafts" "$1/.git/objects" "$1/img"
    printf 'hello\n' >"$1/index.html"
    printf 'abc' >"$1/img/logo.png"
    printf 'one two\n' >"$1/img/photo one.jpg"
    printf 'draft\n' >"$1/docs/old drafts/plan v2.txt"
    printf 'x\n' >"$1/.env"
    printf 'blob\n' >"$1/.git/objects/ab"
}

# tree DIR: every file under DIR, relative, sorted.
tree() {
    (cd "$1" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)
}

# backup ARGS...: runs backup.sh, keeping stdout, stderr and the status in $SCRATCH.
backup() {
    status=0
    sh "$ROOT/backup.sh" "$@" >"$SCRATCH/out" 2>"$SCRATCH/err" || status=$?
}

finish() {
    echo "$PASSED passed, $FAILED failed"
    [ "$FAILED" -eq 0 ]
}
