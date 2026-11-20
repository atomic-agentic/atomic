#!/bin/sh
# Nightly pipeline: extract, validate, transform and load, each logged to OUT_DIR/<stage>.log.
#
#   sh run.sh DATA_DIR [OUT_DIR]
#
# Stops at the first stage that fails. See README.md.

usage() {
    echo "usage: sh run.sh DATA_DIR [OUT_DIR]" >&2
    exit 2
}

[ $# -ge 1 ] && [ $# -le 2 ] || usage
data=$1
out=${2:-out}
[ -d "$data" ] || {
    echo "run.sh: no data directory $data" >&2
    exit 2
}

here=$(cd "$(dirname "$0")" && pwd)
STAGES="extract validate transform load"

mkdir -p "$out" || exit 1
for stage in $STAGES; do
    rm -f "$out/$stage.log"
done

run_stage() {
    stage=$1
    echo "== $stage"
    # The stage's status, not tee's: written to a descriptor around the pipe.
    status=$( { { python3 "$here/$stage.py" "$data" "$out" 2>&1; echo $? >&3; } | tee "$out/$stage.log" >&4; } 3>&1 )
    [ -n "$status" ] || status=1
    if [ "$status" -ne 0 ]; then
        echo "pipeline failed: $stage" >&2
        exit 1
    fi
}

exec 4>&1
for stage in $STAGES; do
    run_stage "$stage"
done
echo "pipeline ok"
