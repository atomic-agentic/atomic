# Messages. say: stdout, unless -q. note: stderr, only with -v. die: stderr, exit 1. usage_error: stderr
# with the usage, exit 2.

VERBOSE=0
QUIET=0

say() {
    [ "$QUIET" -eq 1 ] || printf '%s\n' "$*"
}

note() {
    [ "$VERBOSE" -eq 0 ] || printf 'backup: %s\n' "$*" >&2
}

die() {
    printf 'backup: %s\n' "$*" >&2
    exit 1
}

usage_error() {
    printf 'backup: %s\n' "$*" >&2
    usage >&2
    exit 2
}
