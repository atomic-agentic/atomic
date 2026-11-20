# DEST/MANIFEST: a header, then the copied paths as list_files gives them.
#
#   label: LABEL
#   source: SRC
#   files: N      files copied
#   dirs: D       directories holding at least one copied file, SRC itself counted as .
#   bytes: B      their total size
#   excluded: E   files a -x pattern left out; only when -x was given
#   --
#   path
#   ...

count_lines() {
    wc -l <"$1" | tr -d ' '
}

# total_bytes SRC LIST
total_bytes() {
    total=0
    while IFS= read -r rel; do
        size=$(wc -c <"$1/$rel" | tr -d ' ')
        total=$((total + size))
    done <"$2"
    echo "$total"
}

# count_dirs LIST
count_dirs() {
    while IFS= read -r rel; do
        case "$rel" in
            */*) printf '%s\n' "${rel%/*}" ;;
            *) echo . ;;
        esac
    done <"$1" | LC_ALL=C sort -u | wc -l | tr -d ' '
}

# manifest_field MANIFEST NAME: the value of one header line, empty when there is none.
manifest_field() {
    sed -n -e '/^--$/q' -e "s/^$2: //p" "$1"
}

# manifest_paths MANIFEST: the paths after the header.
manifest_paths() {
    sed '1,/^--$/d' "$1"
}

# write_manifest SRC LIST [EXCLUDED]
write_manifest() {
    printf 'label: %s\n' "$LABEL"
    printf 'source: %s\n' "$1"
    printf 'files: %s\n' "$(count_lines "$2")"
    printf 'dirs: %s\n' "$(count_dirs "$2")"
    printf 'bytes: %s\n' "$(total_bytes "$1" "$2")"
    [ "$EXCLUDING" -eq 0 ] || printf 'excluded: %s\n' "${3:-0}"
    echo --
    cat "$2"
}
