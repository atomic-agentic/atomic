#!/bin/sh
# Checks every deploy file in conf/ but common.conf. Run it from anywhere: sh scripts/check-all.sh
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "${root}/conf" || exit 1
status=0
for file in *.conf; do
    [ "${file}" = common.conf ] && continue
    if PYTHONPATH="${root}" python3 -m deployctl check -q "${file}"; then
        echo "ok    ${file}"
    else
        echo "FAIL  ${file}"
        status=1
    fi
done
exit "${status}"
