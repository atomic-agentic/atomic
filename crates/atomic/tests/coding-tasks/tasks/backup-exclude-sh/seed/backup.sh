#!/bin/sh
# backup: copy a directory's files into a new backup directory, with a MANIFEST. See README.md.
#
#   sh backup.sh [-a] [-q] [-v] [-l LABEL] SRC DEST

LIB=$(cd "$(dirname "$0")/lib" && pwd) || exit 1
. "$LIB/log.sh"
. "$LIB/usage.sh"
. "$LIB/args.sh"
. "$LIB/walk.sh"
. "$LIB/manifest.sh"
. "$LIB/copy.sh"

parse_args "$@"

[ -d "$SRC" ] || die "no source directory $SRC"
if [ -e "$DEST" ] && [ ! -d "$DEST" ]; then
    die "$DEST exists and is not a directory"
fi
if [ -d "$DEST" ] && [ -n "$(ls -A "$DEST")" ]; then
    die "$DEST exists and is not empty"
fi
mkdir -p "$DEST" || die "cannot create $DEST"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/backup.XXXXXX") || die "cannot make a work directory"
trap 'rm -rf "$WORK"' EXIT
trap 'exit 1' INT TERM

list_files "$SRC" >"$WORK/files" || die "cannot list $SRC"
note "$(count_lines "$WORK/files") file(s) to copy from $SRC"
copy_files "$SRC" "$DEST" <"$WORK/files"
write_manifest "$SRC" "$WORK/files" >"$DEST/MANIFEST" || die "cannot write $DEST/MANIFEST"
say "backed up $(count_lines "$WORK/files") file(s), $(total_bytes "$SRC" "$WORK/files") byte(s), to $DEST"
