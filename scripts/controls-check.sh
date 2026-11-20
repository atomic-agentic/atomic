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
# Run it inside scripts/dev.sh with the sandbox available, since some of the
# controls are the sandbox's:
#
#   ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh
#
# It hashes files with git, so in a linked worktree the main checkout's .git
# must be mounted too (ATOMIC_DEV_MOUNTS=<main checkout>/.git); without it
# the script stops before editing anything.
#
# A control whose code has moved, so that its switch no longer applies, fails
# the check: update the switch here with the code. Each control costs a rebuild
# of the crates it is in.
#
# The source is edited in place, so the script guards it. Before any edit it
# copies every file it will touch to target/controls-check/ and checks each
# copy against the file's git hash-object; if that fails, nothing is edited. On
# exit, however it ends, it copies the originals back and checks each file's
# hash again, and fails loudly, keeping the copies, if one does not match. A run
# killed outright leaves target/controls-check/ behind, and the next run refuses
# to start until someone has looked. scripts/controls-check-test.sh tests this.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
FEATURES="atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests"
STATE="${ROOT}/target/controls-check"
LOG="${STATE}/cargo.log"

# name | file | text to replace | replacement | cargo test arguments. A
# control guarded by several tests names them all after `--`; each must fail
# with the control off.
CONTROLS=(
    "git-hardening|crates/atomic-core/src/git.rs|        for setting in HARDENING {|        for setting in HARDENING.iter().take(0) {|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "git-filters|crates/atomic-core/src/git.rs|        let mut overrides = (*self.overrides).clone();|        let mut overrides = (*self.overrides).clone(); let names: Vec<String> = { let _ = names; Vec::new() };|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "git-dir|crates/atomic-core/src/git.rs|            .env(\"GIT_DIR\", &self.git_dir)|            .env(\"ATOMIC_CONTROL_REMOVED\", &self.git_dir)|-p atomic-agentic-core --test git_safety nothing_in_the_worktree_or_the_repository_runs"
    "sandbox-writes|crates/atomic-core/src/codex.rs|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::workspace_write_with(|        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::Disabled, _ if false => PermissionProfile::workspace_write_with(|--workspace --features ${FEATURES} --test sandboxed_commit -- the_turn_cannot_commit_and_atomic_commits_after_it a_project_under_the_system_temporary_directory_stays_out_of_reach"
    "sandbox-tmp|crates/atomic-core/src/codex.rs|            /*exclude_slash_tmp*/ true,|            /*exclude_slash_tmp*/ false,|--workspace --features ${FEATURES} --test sandboxed_commit a_project_under_the_system_temporary_directory_stays_out_of_reach"
    "sandbox-private-state|crates/atomic-core/src/codex.rs|    for p in paths {|    for p in paths.into_iter().take(0) {|--workspace --features ${FEATURES} --test sandboxed_commit --test exec_private_state -- a_read_only_turn_in_a_worktree_sees_its_checkout an_exec_atom_cannot_reach_atomics_private_state"
    "sandbox-network|crates/atomic-core/src/codex.rs|    codex_protocol::permissions::NetworkSandboxPolicy::Restricted;|    codex_protocol::permissions::NetworkSandboxPolicy::Enabled;|--workspace --features ${FEATURES} --test self_approval an_atom_cannot_approve_its_own_gate_through_its_shell"
    "exec-confinement|crates/atomic-exec/src/lib.rs|            Some(confine) => confine.confine(command, &self.tmp),|            Some(confine) if std::hint::black_box(false) => confine.confine(command, &self.tmp), Some(_) => Ok(command.clone()),|--workspace --features ${FEATURES} --test exec_private_state --test run_cli -- an_exec_atom_cannot_reach_atomics_private_state atomic_run_confines_an_exec_atoms_command"
    "signin-personal-only|crates/atomic/src/engine.rs|            (Some(why), true) => {|            (Some(why), true) if std::hint::black_box(false) => {|-p atomic-agentic --test model_auth -- a_shared_store_or_serve_refuses_the_personal_sign_in where_the_sign_in_is_refused_nothing_takes_its_place_and_it_says_what_to_do"
    "process-hardening|crates/atomic-core/src/codex.rs|        codex_process_hardening::pre_main_hardening();|        let _ = codex_process_hardening::pre_main_hardening;|-p atomic-agentic --test model_auth a_running_atomic_cannot_be_read_through_proc_by_its_own_user"
    "signin-restrictions|crates/atomic-core/src/codex.rs|        sign_in.allowed_by(&config.auth_config())?;|        let _ = sign_in;|-p atomic-agentic-core --lib --features fake-model codex_login_restrictions_refuse_a_sign_in_they_do_not_allow"
    "signin-logout-both-stores|crates/atomic-core/src/signin.rs|            AuthCredentialsStoreMode::File,|            AuthCredentialsStoreMode::Keyring,|-p atomic-agentic --test model_auth logout_forgets_the_file_copy_whichever_store_is_chosen"
    "signin-email-masked-text|crates/atomic/src/main.rs|                if let Some(email) = a.email.as_deref().map(atomic_core::signin::mask_email) {|                if let Some(email) = a.email.as_deref() {|-p atomic-agentic --test model_auth login_status_says_who_is_signed_in_and_logout_forgets_it"
    "signin-email-masked-json|crates/atomic/src/main.rs|                    \"email\": a.email.as_deref().map(atomic_core::signin::mask_email),|                    \"email\": a.email.as_deref(),|-p atomic-agentic --test model_auth login_status_says_who_is_signed_in_and_logout_forgets_it"
    "signin-token-overrides|crates/atomic-core/src/codex.rs|        remove(name);|        let _ = name;|-p atomic-agentic-core --lib the_environment_cannot_stand_in_for_or_redirect_a_sign_ins_tokens"
    "codex-home-read-only|crates/atomic-core/src/codex.rs|    for home in homes.into_iter().flatten() {|    for home in homes.into_iter().flatten().take(0) {|-p atomic-agentic-core --lib private_state_is_denied_and_the_rest_of_the_state_directory_is_not"
    "codex-home-files|crates/atomic-core/src/codex.rs|        planted_never(&codex_home.join(name))?;|        let _ = name;|-p atomic-agentic-core --lib private_state_is_denied_and_the_rest_of_the_state_directory_is_not"
)

die() {
    echo "scripts/controls-check.sh: $*" >&2
    exit 1
}

# Fail before any edit, leaving nothing behind.
die_unedited() {
    rm -rf "${STATE}" 2>/dev/null
    die "$*; nothing was edited"
}

# Where a file's original is kept.
copy_of() {
    echo "${STATE}/$(tr / % <<<"$1")"
}

# The controls to check: all of them, or the one named.
only="${1:-}"
selected=()
for row in "${CONTROLS[@]}"; do
    name="${row%%|*}"
    if [ -z "${only}" ] || [ "${only}" = "${name}" ]; then
        selected+=("${row}")
    fi
done
if [ "${#selected[@]}" -eq 0 ]; then
    echo "scripts/controls-check.sh: no control is named '${only}'. The controls:" >&2
    for row in "${CONTROLS[@]}"; do
        echo "  ${row%%|*}" >&2
    done
    exit 2
fi

if [ -e "${STATE}" ]; then
    die "${STATE} is left from a run that did not finish. Its manifest names each file that run edited, with the file's original hash, and the directory holds the originals: compare them with the files (git diff), put them back, then remove ${STATE}."
fi

# Save every file the selected controls edit, and check each copy, before
# editing anything.
mkdir -p "${STATE}" || die_unedited "cannot create ${STATE}"
: >"${STATE}/manifest" || die_unedited "cannot write ${STATE}/manifest"
recorded=" "
for row in "${selected[@]}"; do
    IFS='|' read -r _ file _ <<<"${row}"
    case "${recorded}" in *" ${file} "*) continue ;; esac
    hash="$(git hash-object -- "${file}")" || die_unedited "cannot hash ${file}"
    copy="$(copy_of "${file}")"
    cp -- "${file}" "${copy}" || die_unedited "cannot save ${file}"
    [ "$(git hash-object -- "${copy}")" = "${hash}" ] ||
        die_unedited "the copy of ${file} does not match it"
    echo "${hash} ${file}" >>"${STATE}/manifest" || die_unedited "cannot record ${file}"
    recorded+="${file} "
done

# Put every saved file back and check it. A file that cannot be put back is
# named, and the originals stay.
restore() {
    local bad=0 hash file
    if [ ! -r "${STATE}/manifest" ]; then
        echo "scripts/controls-check.sh: ${STATE}/manifest cannot be read, so no file can be checked. Compare the files with git and ${STATE} by hand." >&2
        return 1
    fi
    while read -r hash file; do
        cp -- "$(copy_of "${file}")" "${file}" 2>/dev/null
        if [ "$(git hash-object -- "${file}" 2>/dev/null)" != "${hash}" ]; then
            echo "scripts/controls-check.sh: ${file} IS LEFT MODIFIED: it does not match its original, ${hash}. Put it back by hand; ${STATE} keeps what was saved." >&2
            bad=1
        fi
    done <"${STATE}/manifest"
    if [ "${bad}" -eq 0 ]; then
        rm -rf "${STATE}"
    fi
    return "${bad}"
}
on_exit() {
    local code=$?
    # Nothing interrupts putting the files back.
    trap '' INT TERM HUP
    restore || code=1
    exit "${code}"
}
trap on_exit EXIT

# cargo runs in the background so that a signal reaches the script while it
# waits (bash would hold a signal until a foreground command returned, and a
# time limit's later SIGKILL would then leave a control switched off). The
# signal stops cargo, and the exit puts the files back.
cargo_pid=""
on_signal() {
    trap '' INT TERM HUP
    [ -n "${cargo_pid}" ] && kill -TERM "${cargo_pid}" 2>/dev/null
    echo "scripts/controls-check.sh: interrupted; putting the files back" >&2
    exit 130
}
trap on_signal INT TERM HUP
run_cargo() {
    # shellcheck disable=SC2086
    cargo test --locked ${1} >"${LOG}" 2>&1 &
    cargo_pid=$!
    wait "${cargo_pid}"
    local code=$?
    cargo_pid=""
    return "${code}"
}
# Keep a failing control's cargo output, which the exit would delete.
keep_log() {
    cp -- "${LOG}" "${ROOT}/target/controls-check-$1.log" 2>/dev/null &&
        echo "  cargo's output: target/controls-check-$1.log" >&2
}

# Switch a control off: replace the first `old` in `file` with `new`.
switch_off() {
    python3 - "$1" "$2" "$3" <<'PY'
import sys
path, old, new = sys.argv[1:]
text = open(path).read()
open(path, "w").write(text.replace(old, new, 1))
PY
}

status=0
for row in "${selected[@]}"; do
    IFS='|' read -r name file text replacement args <<<"${row}"
    if ! grep -qF -- "${text}" "${file}"; then
        echo "FAIL ${name}: its switch no longer applies to ${file}; update scripts/controls-check.sh" >&2
        status=1
        continue
    fi
    switch_off "${file}" "${text}" "${replacement}" || die "cannot switch ${name} off"
    run_cargo "${args}"
    removed=$?
    cp -- "$(copy_of "${file}")" "${file}" || die "cannot put ${file} back"
    if grep -q "^error\(\[E[0-9]*\]\)\?: could not compile\|^error: could not compile" "${LOG}"; then
        echo "FAIL ${name}: with the control off the code does not build" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    if ! grep -q "^test result:" "${LOG}"; then
        echo "FAIL ${name}: no test ran" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    # Every test the row names must fail with the control off.
    passed="$(awk '/^test result:/ { p += $4 } END { print p + 0 }' "${LOG}")"
    if [ "${removed}" -eq 0 ] || [ "${passed}" -ne 0 ] || ! grep -q "^test .* FAILED$" "${LOG}"; then
        echo "FAIL ${name}: its test passed with the control off" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    if ! run_cargo "${args}"; then
        echo "FAIL ${name}: its test fails with the control on" >&2
        grep -E "^test .* FAILED$|panicked at" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    echo "ok ${name}: its test fails with the control off and passes with it on" >&2
done
exit "${status}"
