#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A repeatable -x for a POSIX sh tool: options, file list, copy, MANIFEST, help and README all agree.
source "$(dirname "$0")/../../lib/check-lib.sh"
data="$(mktemp -d "${TMPDIR:-/tmp}/atomic-task-data.XXXXXX")"
trap 'rm -rf "${G}" "${data}"' EXIT
src="${data}/src"
mkdir -p "${src}/sub dir" "${src}/cache/nested" "${src}/cachet" "${src}/docs"
printf 'r\n' >"${src}/report.txt"
printf 'log1\n' >"${src}/app.log"
printf 'deep\n' >"${src}/sub dir/deep file.log"
printf 'keep\n' >"${src}/sub dir/keep me.txt"
printf 'xx' >"${src}/cache/x.bin"
printf 'yyy' >"${src}/cache/nested/y.bin"
printf 'z\n' >"${src}/cachet/z.txt"
printf 'e\n' >"${src}/.env.log"
printf '# d\n' >"${src}/docs/readme.md"

# run NAME TOOL ARGS...: TOOL's stdout, stderr and status under ${data}/NAME.
run() {
    local name="$1" tool="$2"
    shift 2
    local code=0
    (cd "${data}" && timeout -k 5 30 sh "${tool}" "$@" >"${data}/${name}.out" 2>"${data}/${name}.err") || code=$?
    echo "${code}" >"${data}/${name}.status"
}
status() { cat "${data}/$1.status"; }
tree() { (cd "$1" && find . -type f | sed 's|^\./||' | LC_ALL=C sort); }
manifest() {
    printf '%s\n' "label: $1" "source: ${src}" "files: $2" "dirs: $3" "bytes: $4" "excluded: $5" -- "${@:6}"
}

timeout -k 5 60 sh tests/run.sh >&2 || fail "the tests in tests/run.sh fail"

# Without -x, exactly as before, against the seed's own scripts.
for opts in "" "-a -v -l weekly"; do
    # shellcheck disable=SC2086
    run seed "${SEED}/backup.sh" ${opts} src seed-dest
    # shellcheck disable=SC2086
    run agent "${G}/backup.sh" ${opts} src agent-dest
    for part in out status; do
        [ "$(sed "s/seed-dest/DEST/g" "${data}/seed.${part}")" = "$(sed "s/agent-dest/DEST/g" "${data}/agent.${part}")" ] \
            || fail "without -x, '${opts}' gives a different ${part}: $(cat "${data}/agent.${part}")"
    done
    [ "$(sed "s/seed-dest/DEST/g" "${data}/seed.err" | LC_ALL=C sort)" = "$(sed "s/agent-dest/DEST/g" "${data}/agent.err" | LC_ALL=C sort)" ] \
        || fail "without -x, '${opts}' prints a different stderr: $(cat "${data}/agent.err")"
    diff -r "${data}/seed-dest" "${data}/agent-dest" >&2 || fail "without -x, '${opts}' makes a different backup"
    rm -rf "${data}/seed-dest" "${data}/agent-dest"
done
src="src"

run a "${G}/backup.sh" -l nightly -x '*.log' -x 'cache/*' -x '*.nomatch' src a
[ "$(status a)" = 0 ] || fail "backup.sh -x failed: $(cat "${data}/a.err")"
[ "$(tree "${data}/a")" = "$(printf '%s\n' MANIFEST cachet/z.txt docs/readme.md report.txt 'sub dir/keep me.txt')" ] \
    || fail "-x '*.log' -x 'cache/*' copied: $(tree "${data}/a" | tr '\n' '|')"
[ "$(cat "${data}/a/MANIFEST")" = "$(manifest nightly 4 4 13 4 cachet/z.txt docs/readme.md report.txt 'sub dir/keep me.txt')" ] \
    || fail "-x '*.log' -x 'cache/*' wrote this MANIFEST: $(cat "${data}/a/MANIFEST")"
[ "$(cat "${data}/a.out")" = "backed up 4 file(s), 13 byte(s), to a" ] || fail "-x printed: $(cat "${data}/a.out")"

run b "${G}/backup.sh" -a -x '*.log' src b
[ "$(cat "${data}/b/MANIFEST" 2>&1)" = "$(manifest backup 6 6 18 3 cache/nested/y.bin cache/x.bin cachet/z.txt docs/readme.md report.txt 'sub dir/keep me.txt')" ] \
    || fail "-a -x '*.log' wrote this MANIFEST: $(cat "${data}/b/MANIFEST" 2>&1)"

run c "${G}/backup.sh" -x 'sub dir/*' -l weekly src c
[ "$(cat "${data}/c/MANIFEST" 2>&1)" = "$(manifest weekly 6 5 18 2 app.log cache/nested/y.bin cache/x.bin cachet/z.txt docs/readme.md report.txt)" ] \
    || fail "-x 'sub dir/*' -l weekly wrote this MANIFEST: $(cat "${data}/c/MANIFEST" 2>&1)"

run d "${G}/backup.sh" -x '*.nomatch' src d
[ "$(cat "${data}/d/MANIFEST" 2>&1)" = "$(manifest backup 8 6 28 0 app.log cache/nested/y.bin cache/x.bin cachet/z.txt docs/readme.md report.txt 'sub dir/deep file.log' 'sub dir/keep me.txt')" ] \
    || fail "-x with a pattern that matches nothing wrote this MANIFEST: $(cat "${data}/d/MANIFEST" 2>&1)"

run e "${G}/backup.sh" -x
[ "$(status e)" = 2 ] || fail "-x with no pattern exited $(status e), not 2"
run e "${G}/backup.sh" -x '*.log' src
[ "$(status e)" = 2 ] || fail "-x with no DEST exited $(status e), not 2"

run r "${G}/restore.sh" -q a restored
[ "$(status r)" = 0 ] || fail "restore.sh refuses a backup made with -x: $(cat "${data}/r.err")"
[ "$(tree "${data}/restored")" = "$(printf '%s\n' cachet/z.txt docs/readme.md report.txt 'sub dir/keep me.txt')" ] \
    || fail "restore.sh restored: $(tree "${data}/restored" | tr '\n' '|')"

run h "${G}/backup.sh" -h
[ "$(status h)" = 0 ] && grep -q -- '-x PATTERN' "${data}/h.out" || fail "backup.sh -h does not show -x PATTERN"
grep -q -- '-x PATTERN' README.md || fail "README.md does not document -x PATTERN"
