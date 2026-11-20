# Copy each listed file (paths on stdin, relative to SRC) into DEST, making directories as needed, and
# keeping modes and times.

copy_files() {
    while IFS= read -r rel; do
        case "$rel" in
            */*) mkdir -p "$2/${rel%/*}" || die "cannot create $2/${rel%/*}" ;;
        esac
        cp -p "$1/$rel" "$2/$rel" || die "cannot copy $rel"
        note "copied $rel"
    done
}
