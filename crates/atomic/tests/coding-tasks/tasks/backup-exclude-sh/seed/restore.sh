#!/bin/sh
# restore: check a backup made by backup.sh against its MANIFEST, then copy its files into TARGET.
#
#   sh restore.sh [-q] BACKUP TARGET
#
# TARGET must be new or empty. Nothing is copied when the check fails.

LIB=$(cd "$(dirname "$0")/lib" && pwd) || exit 1
. "$LIB/log.sh"
. "$LIB/manifest.sh"
. "$LIB/copy.sh"

usage() {
    echo "usage: sh restore.sh [-q] BACKUP TARGET"
}

if [ "${1:-}" = -q ]; then
    QUIET=1
    shift
fi
[ $# -eq 2 ] || usage_error "give BACKUP and TARGET"
backup=$1
target=$2
manifest=$backup/MANIFEST
[ -f "$manifest" ] || die "no MANIFEST in $backup"
if [ -d "$target" ] && [ -n "$(ls -A "$target")" ]; then
    die "$target exists and is not empty"
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/restore.XXXXXX") || die "cannot make a work directory"
trap 'rm -rf "$WORK"' EXIT
trap 'exit 1' INT TERM

manifest_paths "$manifest" >"$WORK/files"
listed=$(count_lines "$WORK/files")
[ "$(manifest_field "$manifest" files)" = "$listed" ] ||
    die "MANIFEST says $(manifest_field "$manifest" files) file(s) but lists $listed"
[ "$(manifest_field "$manifest" dirs)" = "$(count_dirs "$WORK/files")" ] ||
    die "MANIFEST's dirs does not match its paths"
while IFS= read -r rel; do
    [ -f "$backup/$rel" ] || die "MANIFEST lists $rel, which is not in $backup"
done <"$WORK/files"
[ "$(manifest_field "$manifest" bytes)" = "$(total_bytes "$backup" "$WORK/files")" ] ||
    die "MANIFEST's bytes does not match the files"

mkdir -p "$target" || die "cannot create $target"
copy_files "$backup" "$target" <"$WORK/files"
say "restored $listed file(s) to $target"
