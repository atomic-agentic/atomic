#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Run the test suites behind each milestone that has landed, and report which
# pass, as a Markdown table. A Codex bump uses this to say what still works at
# a candidate revision.
#
#   scripts/milestone-tests.sh            # table on stdout; exit 1 if any row fails
#
# The table is also appended to $GITHUB_STEP_SUMMARY when that is set. The
# rows that include atomic-agentic-durable run its Postgres variants too, so
# they need the test database in ATOMIC_TEST_DATABASE_URL (scripts/dev.sh
# starts one); without it those rows fail, naming it, rather than passing with
# the Postgres variants unrun. Every row runs even when an earlier one fails;
# the last row runs the whole workspace, so a test no row names still counts.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
POSTGRES="--features atomic-agentic/postgres-tests"

# milestone | what it covers | cargo test arguments
ROWS=(
    "M0|durable core: kill spike, step positions, executor identity, publication, store|-p atomic-agentic-durable"
    "M1|the atomic binary and its Codex entry point|-p atomic-agentic -- --skip every_failpoint_triggered_in_turn"
    "M2|typed atoms on Codex and lite, repair turn, fake model|-p atomic-agentic-core --lib --features fake-model -- codex:: lite:: runtime:: typed:: testing::"
    "M2|golden test: the built-in atoms on the fake model|-p atomic-agentic-atoms"
    "M3|durable runtime, candidates, publication, run_atom|-p atomic-agentic-durable --lib -- runtime:: publication::"
    "M3|two-atom workflow: kill and restart, panicking atom|-p atomic-agentic --test change_request"
    "M11a|declarative atoms: load, compile, render, workflows|-p atomic-agentic-core --lib -- atom:: compile:: render:: tags:: workflow:: collections::"
    "M11a|schemas: YAML, registry, lowering, CEL|-p atomic-agentic-schema -- yaml:: registry:: lower:: expr::"
    "M35a|artifact store and typed references|-p atomic-agentic-core --lib -- artifact::"
    "M35a|artifact references in schemas|-p atomic-agentic-schema -- artifact::"
    "-|project, configuration and state|-p atomic-agentic-core --lib -- project:: config:: state::"
    "M38a|failpoints and the crash suite (needs the sandbox)|-p atomic-agentic --features sandbox-tests --test failpoints --test crash"
    "all|the whole workspace, but the crash suite (M38a)|--workspace -- --skip every_failpoint_triggered_in_turn"
)

table="| Milestone | Suite | Result | Passed | Failed |
| --- | --- | --- | --- | --- |"
status=0
LOG="$(mktemp)"
trap 'rm -f "${LOG}"' EXIT

for row in "${ROWS[@]}"; do
    IFS='|' read -r milestone what args <<<"${row}"
    if [[ "${args}" == *" -- "* ]]; then
        cargo_args="${args%% -- *}"
        test_args="${args#* -- }"
    else
        cargo_args="${args}"
        test_args=""
    fi
    case " ${cargo_args} " in
        *" -p atomic-agentic-durable "* | *" --workspace "*) cargo_args+=" ${POSTGRES}" ;;
    esac
    # shellcheck disable=SC2086
    cargo test --locked --no-fail-fast ${cargo_args} -- ${test_args} >"${LOG}" 2>&1
    code=$?
    passed="$(awk '/^test result:/ { p += $4 } END { print p + 0 }' "${LOG}")"
    failed="$(awk '/^test result:/ { f += $6 } END { print f + 0 }' "${LOG}")"
    if [ "${code}" -eq 0 ]; then
        result="pass"
    elif grep -q "^error\(\[E[0-9]*\]\)\?: could not compile\|^error: could not compile" "${LOG}"; then
        result="does not build"
        status=1
    else
        result="FAIL"
        status=1
        # Name the failing tests under the table.
        names="$(grep -E '^test .* FAILED$' "${LOG}")"
        [ -n "${names}" ] || names="$(grep -m1 -E '^error' "${LOG}")"
        failures+="$(sed "s/^/${milestone}: /" <<<"${names}")"$'\n'
    fi
    table+=$'\n'"| ${milestone} | ${what} | ${result} | ${passed} | ${failed} |"
    echo "${milestone} ${what}: ${result} (${passed} passed, ${failed} failed)" >&2
done

report="${table}"
if [ -n "${failures:-}" ]; then
    report+=$'\n\nFailing tests:\n\n```\n'"${failures}"'```'
fi
echo "${report}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo "${report}" >>"${GITHUB_STEP_SUMMARY}"
fi
exit "${status}"
