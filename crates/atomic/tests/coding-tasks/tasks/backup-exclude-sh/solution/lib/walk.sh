# The files to back up: one path a line, relative to SRC, without a leading ./, sorted bytewise.
# Dot files, and everything under a dot directory, are left out unless -a was given (ALL=1).
# Names holding a newline are not supported (README.md).

list_files() {
    if [ "$ALL" -eq 1 ]; then
        (cd "$1" && find . -type f)
    else
        (cd "$1" && find . -name '.?*' -prune -o -type f -print)
    fi | sed 's|^\./||' | LC_ALL=C sort
}

# is_excluded PATH: whether a -x pattern matches PATH, as a case pattern does, so * matches / too.
is_excluded() {
    [ "$EXCLUDING" -eq 1 ] || return 1
    saved_ifs=$IFS
    IFS=$NL
    set -f
    for pattern in $EXCLUDES; do
        case "$1" in
            $pattern)
                IFS=$saved_ifs
                set +f
                return 0
                ;;
        esac
    done
    IFS=$saved_ifs
    set +f
    return 1
}

# exclude_files: the paths on stdin that no -x pattern matches, in the same order.
exclude_files() {
    while IFS= read -r rel; do
        is_excluded "$rel" || printf '%s\n' "$rel"
    done
}
