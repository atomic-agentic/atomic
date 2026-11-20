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
