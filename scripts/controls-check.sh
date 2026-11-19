#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Controls are tested by removing them. For each v0.1 control, this switches
# the control off in the source, runs the test that guards it and requires that
# test to fail, then puts the source back and requires it to pass again.
#
#   scripts/controls-check.sh            # every control
#   scripts/controls-check.sh git-dir    # one, by name
#
# Run it inside scripts/dev.sh with the sandbox available, since two controls
# are the sandbox's:
#
#   ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh
#
# A control whose code has moved, so that its switch no longer applies, fails
# the check: update the switch here with the code. The source is restored on
# exit, whatever happens; each control costs a rebuild of the crates it is in.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
FEATURES="atomic-agentic-durable/postgres-tests,atomic-agentic/sandbox-tests"
LOG="$(mktemp)"
SAVED="$(mktemp -d)"
restore() {
    for f in "${SAVED}"/*; do
        [ -e "${f}" ] || continue
        cp "${f}" "$(sed 's|%|/|g' <<<"$(basename "${f}")")"
    done
    rm -rf "${SAVED}" "${LOG}"
}
trap restore EXIT

# name | file | text to replace | replacement | cargo test arguments
CONTROLS=(
    "git-hardening|crates/atomic-core/src/git.rs|        for setting in HARDENING {|        for setting in HARDENING.iter().take(0) {|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "git-filters|crates/atomic-core/src/git.rs|        let mut overrides = (*self.overrides).clone();|        let mut overrides = (*self.overrides).clone(); let names: Vec<String> = { let _ = names; Vec::new() };|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "git-dir|crates/atomic-core/src/git.rs|            .env(\"GIT_DIR\", &self.git_dir)|            .env(\"ATOMIC_CONTROL_REMOVED\", &self.git_dir)|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "sandbox-state-dir|crates/atomic-core/src/codex.rs|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::workspace_write(),|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::Disabled,|--workspace --features ${FEATURES} --test sandboxed_commit the_turn_cannot_write_the_state_directory"
    "sandbox-git-dir|crates/atomic-core/src/codex.rs|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::workspace_write(),|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::Disabled,|--workspace --features ${FEATURES} --test sandboxed_commit the_turn_cannot_commit_and_atomic_commits_after_it"
)

status=0
only="${1:-}"
for row in "${CONTROLS[@]}"; do
    IFS='|' read -r name file text replacement args <<<"${row}"
    [ -z "${only}" ] || [ "${only}" = "${name}" ] || continue
    saved="${SAVED}/$(sed 's|/|%|g' <<<"${file}")"
    [ -e "${saved}" ] || cp "${file}" "${saved}"
    if ! grep -qF -- "${text}" "${file}"; then
        echo "FAIL ${name}: its switch no longer applies to ${file}; update scripts/controls-check.sh" >&2
        status=1
        continue
    fi
    python3 - "${file}" "${text}" "${replacement}" <<'EOF'
import sys
path, old, new = sys.argv[1:]
text = open(path).read()
open(path, "w").write(text.replace(old, new, 1))
EOF
    # shellcheck disable=SC2086
    cargo test --locked ${args} >"${LOG}" 2>&1
    removed=$?
    cp "${saved}" "${file}"
    if grep -q "^error\(\[E[0-9]*\]\)\?: could not compile\|^error: could not compile" "${LOG}"; then
        echo "FAIL ${name}: with the control off the code does not build" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        status=1
        continue
    fi
    if ! grep -q "^test result:" "${LOG}"; then
        echo "FAIL ${name}: no test ran" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        status=1
        continue
    fi
    if [ "${removed}" -eq 0 ] || ! grep -q "^test .* FAILED$" "${LOG}"; then
        echo "FAIL ${name}: its test passed with the control off" >&2
        status=1
        continue
    fi
    # shellcheck disable=SC2086
    if ! cargo test --locked ${args} >"${LOG}" 2>&1; then
        echo "FAIL ${name}: its test fails with the control on" >&2
        grep -E "^test .* FAILED$|panicked at" "${LOG}" >&2
        status=1
        continue
    fi
    echo "ok ${name}: its test fails with the control off and passes with it on" >&2
done
exit "${status}"
