#!/bin/sh
# The tests for backup.sh and restore.sh. Exit 0 when every check passes.
#
#   sh tests/run.sh

. "$(cd "$(dirname "$0")" && pwd)/helpers.sh"

src=$SCRATCH/site
sample "$src"

backup -l nightly "$src/" "$SCRATCH/b1"
check "a backup works" 0 "$status"
check "and says what it did" "backed up 4 file(s), 23 byte(s), to $SCRATCH/b1" "$(cat "$SCRATCH/out")"
check "it copies every file but dot files" "$(printf '%s\n' MANIFEST 'docs/old drafts/plan v2.txt' img/logo.png 'img/photo one.jpg' index.html)" \
    "$(tree "$SCRATCH/b1")"
check "with the same contents" "one two" "$(cat "$SCRATCH/b1/img/photo one.jpg")"
check "and writes its MANIFEST" "$(printf '%s\n' 'label: nightly' "source: $src" 'files: 4' 'dirs: 3' 'bytes: 23' -- \
    'docs/old drafts/plan v2.txt' img/logo.png 'img/photo one.jpg' index.html)" "$(cat "$SCRATCH/b1/MANIFEST")"

backup -a -q "$src" "$SCRATCH/b2"
check "-a -q works" 0 "$status"
check "-q prints nothing" "" "$(cat "$SCRATCH/out" "$SCRATCH/err")"
check "-a copies dot files" "$(printf '%s\n' .env .git/objects/ab)" "$(sed '1,/^--$/d' "$SCRATCH/b2/MANIFEST" | grep '^\.')"
check "the default label" "label: backup" "$(head -n 1 "$SCRATCH/b2/MANIFEST")"
check "dirs counts directories that hold files" "dirs: 4" "$(grep '^dirs:' "$SCRATCH/b2/MANIFEST")"

backup -v "$src" "$SCRATCH/b3"
check "-v names each file" "backup: copied img/logo.png" "$(grep 'logo' "$SCRATCH/err")"

backup "$src" "$SCRATCH/b1"
check "a full DEST is refused" 1 "$status"
check "with a message" "backup: $SCRATCH/b1 exists and is not empty" "$(cat "$SCRATCH/err")"

backup "$SCRATCH/nowhere" "$SCRATCH/b4"
check "a missing SRC fails" 1 "$status"

backup -z "$src" "$SCRATCH/b4"
check "an unknown option is a usage error" 2 "$status"
check "with the usage" "usage: sh backup.sh [-a] [-q] [-v] [-l LABEL] [-x PATTERN]... SRC DEST" "$(sed -n 2p "$SCRATCH/err")"

backup -l
check "-l with no label is a usage error" 2 "$status"

backup -h
check "-h is not an error" 0 "$status"
check "and prints the usage" "usage: sh backup.sh [-a] [-q] [-v] [-l LABEL] [-x PATTERN]... SRC DEST" "$(head -n 1 "$SCRATCH/out")"

mkdir -p "$SCRATCH/empty"
backup "$SCRATCH/empty" "$SCRATCH/b5"
check "an empty SRC gives an empty backup" "$(printf '%s\n' 'label: backup' "source: $SCRATCH/empty" 'files: 0' 'dirs: 0' 'bytes: 0' --)" \
    "$(cat "$SCRATCH/b5/MANIFEST")"

backup -x '*.png' -x 'docs/*' -x 'nothing*' "$src" "$SCRATCH/b6"
check "-x works" 0 "$status"
check "-x leaves files out of the copy" "$(printf '%s\n' MANIFEST 'img/photo one.jpg' index.html)" "$(tree "$SCRATCH/b6")"
check "and out of MANIFEST, which counts them" "$(printf '%s\n' 'label: backup' "source: $src" 'files: 2' 'dirs: 2' 'bytes: 14' \
    'excluded: 2' -- 'img/photo one.jpg' index.html)" "$(cat "$SCRATCH/b6/MANIFEST")"
backup -x 'img/photo *' "$src" "$SCRATCH/b7"
check "a pattern with a space" "files: 3|excluded: 1" "$(grep -E '^(files|excluded):' "$SCRATCH/b7/MANIFEST" | paste -sd '|' -)"
backup -x
check "-x with no pattern is a usage error" 2 "$status"

status=0
sh "$ROOT/restore.sh" -q "$SCRATCH/b6" "$SCRATCH/r6" || status=$?
check "restore takes a backup made with -x" 0 "$status"

status=0
sh "$ROOT/restore.sh" -q "$SCRATCH/b1" "$SCRATCH/r1" || status=$?
check "restore works" 0 "$status"
check "and puts back every file" "$(sed '1,/^--$/d' "$SCRATCH/b1/MANIFEST")" "$(tree "$SCRATCH/r1")"

printf 'more\n' >>"$SCRATCH/b3/index.html"
status=0
sh "$ROOT/restore.sh" "$SCRATCH/b3" "$SCRATCH/r2" 2>"$SCRATCH/err" || status=$?
check "restore refuses a backup that does not match its MANIFEST" 1 "$status"
check "and copies nothing" "" "$(ls -A "$SCRATCH/r2" 2>/dev/null)"

finish
