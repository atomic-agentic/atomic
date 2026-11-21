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
# The source is edited in place, so the script guards it. It refuses to start
# when a file a control names differs from HEAD, because it would restore that
# dirty state and read as clean. Before any edit it copies every file it will
# touch to target/controls-check/ and checks each copy against the file's git
# hash-object; if that fails, nothing is edited. On exit, however it ends, it
# copies the originals back, checks each file's hash again and asks git that the
# file no longer differs from HEAD, and fails loudly, keeping the copies, if
# either fails. Each file put back is touched, so cargo rebuilds it rather than
# reusing an artefact built with the control off. A run killed outright leaves
# target/controls-check/ behind, and the next run refuses to start until someone
# has looked. scripts/controls-check-test.sh tests this.
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
    "sandbox-second-mounts|crates/atomic-core/src/codex.rs|    all.extend(aliases.iter().cloned());|    let _ = aliases; // CONTROL_OFF|--workspace --features ${FEATURES} --test second_mounts a_denied_path_is_denied_at_a_second_mount_of_its_directory"
    "sandbox-second-mounts-cap|crates/atomic-core/src/codex.rs|    if link_count + second > MAX_LINK_TARGETS {|    if std::hint::black_box(false) && link_count + second > MAX_LINK_TARGETS {|-p atomic-agentic-core --lib --features fake-model a_second_mount_that_cannot_be_denied_refuses_the_list"
    "sandbox-second-mounts-needed|crates/atomic-core/src/codex.rs|        if let Some(needed) = covers(alias) {|        if let Some(needed) = covers(alias).and(None::<PathBuf>) {|-p atomic-agentic-core --lib --features fake-model a_second_mount_that_cannot_be_denied_refuses_the_list"
    "sandbox-second-mounts-movable|crates/atomic-core/src/codex.rs|        if two_deep(alias, &writable) {|        if std::hint::black_box(false) && two_deep(alias, &writable) {|-p atomic-agentic-core --lib --features fake-model a_second_mount_that_cannot_be_denied_refuses_the_list"
    "sandbox-second-mounts-home-checked|crates/atomic-core/src/codex.rs|    for alias in aliases.iter().chain(home_aliases) {|    for alias in aliases.iter() { let _ = home_aliases; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_second_mount_that_cannot_be_denied_refuses_the_list"
    "sandbox-mounts-namespace-files|crates/atomic-core/src/mounts.rs|                root: root.is_absolute().then_some(root),|                root: Some(root.is_absolute().then_some(root)?), // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_real_table_parses_and_finds_what_it_should"
    "sandbox-mounts-orphan|crates/atomic-core/src/mounts.rs|        if let Some(orphan) = mounts|        if let Some(orphan) = Vec::<Mount>::new() // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_mount_on_one_the_table_does_not_list_is_refused"
    "sandbox-mounts-lost-roots|crates/atomic-core/src/mounts.rs|    if may_lose_roots(&home.kind)|    if std::hint::black_box(false) && may_lose_roots(&home.kind)|-p atomic-agentic-core --lib --features fake-model a_lost_fuse_root_is_refused"
    "sandbox-second-mounts-home-counted|crates/atomic-core/src/codex.rs|    let second = aliases.len() + home_aliases.len();|    let second = aliases.len(); let _ = home_aliases; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_second_mount_that_cannot_be_denied_refuses_the_list"
    "sandbox-mounts-network|crates/atomic-core/src/mounts.rs|    if unknowable(&home.kind) {|    if std::hint::black_box(false) && unknowable(&home.kind) {|-p atomic-agentic-core --lib --features fake-model a_network_filesystem_mounted_twice_is_refused"
    "sandbox-mounts-stacked|crates/atomic-core/src/mounts.rs|            here = top(mounts, child);|            here = child; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model the_mount_at_a_path_is_the_top_of_its_stack"
    "sandbox-mounts-inside|crates/atomic-core/src/mounts.rs|            aliases_of(mounts, &inner.at, &mut out)?;|            let _ = inner; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_mount_inside_a_denied_directory_is_followed_to_its_other_mounts"
    "turn-tmp|crates/atomic-core/src/codex.rs|            std::env::set_var(\"TMPDIR\", own);|            let _ = own;|--workspace --features ${FEATURES} --test turn_workspace concurrent_turns_in_one_directory_never_break_each_others_sandbox"
    "helper-stage|crates/atomic-core/src/codex.rs|    arg != \"--\"|    { let _ = arg; true }|--workspace --features ${FEATURES} --test turn_workspace a_command_naming_the_inner_stage_leaves_the_registry_shared"
    "exec-confinement|crates/atomic-exec/src/lib.rs|            Some(confine) => confine.confine_held(command, &self.tmp),|            Some(confine) if std::hint::black_box(false) => confine.confine_held(command, &self.tmp), Some(_) => Ok((command.clone(), None)),|--workspace --features ${FEATURES} --test exec_private_state --test run_cli -- an_exec_atom_cannot_reach_atomics_private_state atomic_run_confines_an_exec_atoms_command a_confined_command_has_no_controlling_terminal"
    "sandbox-network|crates/atomic-core/src/codex.rs|    codex_protocol::permissions::NetworkSandboxPolicy::Restricted;|    codex_protocol::permissions::NetworkSandboxPolicy::Enabled;|--workspace --features ${FEATURES} --test self_approval --test exec_network -- an_atom_cannot_approve_its_own_gate_through_its_shell a_confined_command_reaches_only_the_hosts_it_is_given"
    "exec-network-attribution|crates/atomic-core/src/network.rs|        let decision = if req.execution_id.is_none() {|        let decision = if /*CONTROL_OFF*/ false {|-p atomic-agentic-core --lib --features fake-model network::tests::only_a_commands_bridge_can_use_its_proxy"
    "exec-network-embedded-ipv4|crates/atomic-core/src/network.rs|const EMBEDDING_IPV4: &[&str] = &[\"64:ff9b:*\", \"2002:*:*\", \"2001:0:*\", \"2001::*\"];|const EMBEDDING_IPV4: &[&str] = &[];|-p atomic-agentic-core --lib --features fake-model network::tests::embedded_ipv4_and_this_machines_addresses_are_never_reached"
    "exec-network-own-addresses|crates/atomic-core/src/network.rs|        .chain(own.iter().map(IpAddr::to_string))|        .chain(own.iter().take(0).map(IpAddr::to_string))|-p atomic-agentic-core --lib --features fake-model network::tests::embedded_ipv4_and_this_machines_addresses_are_never_reached"
    "exec-network-own-wired|crates/atomic-core/src/network.rs|        let own = self.own()?;|        let own: BTreeSet<IpAddr> = BTreeSet::new(); // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model network::tests::a_prepared_proxy_refuses_this_machines_addresses"
    "exec-network-own-reuse|crates/atomic-core/src/network.rs|        self.own == *own && alive(self)|        alive(self)|-p atomic-agentic-core --lib --features fake-model network::tests::a_proxy_is_not_reused_once_this_machines_addresses_change"
    "exec-network-own-nonempty|crates/atomic-core/src/network.rs|        !own.is_empty(),|        true,|-p atomic-agentic-core --lib --features fake-model network::tests::this_machines_addresses_come_from_its_interfaces"
    "exec-network-public-suffix|crates/atomic-core/src/network.rs|    if lower.starts_with(\"*.\") && public_suffix(name) {|    if /*CONTROL_OFF*/ false {|-p atomic-agentic-core --lib --features fake-model network::tests::a_wildcard_over_a_public_suffix_is_refused"
    "exec-network-suffix-rule-parent|crates/atomic-core/src/network.rs|    is_suffix(&format!(\"atomic-probe.{name}\"))|    false|-p atomic-agentic-core --lib --features fake-model network::tests::a_wildcard_over_a_public_suffix_is_refused"
    "exec-network-wildcard-one-site|crates/atomic-core/src/network.rs|                    && within_one_site(&host, parent)|                    && true|-p atomic-agentic-core --lib --features fake-model network::tests::a_wildcard_does_not_reach_across_a_public_suffix_below_it"
    "exec-network-allowlist|crates/atomic-core/src/network.rs|        } else if !declared(&self.0, &req.host) {|        } else if /*CONTROL_OFF*/ false {|--workspace --features ${FEATURES} --test run_cli an_exec_atom_reaches_only_the_hosts_it_declares"
    "exec-network-ports|crates/atomic-core/src/network.rs|        } else if !PORTS.contains(&req.port) {|        } else if /*CONTROL_OFF*/ false {|--workspace --features ${FEATURES} --test run_cli an_exec_atom_reaches_only_the_hosts_it_declares"
    "exec-network-upstream|crates/atomic-core/src/network.rs|        allow_upstream_proxy: false,|        allow_upstream_proxy: /*CONTROL_OFF*/ true,|-p atomic-agentic-core --lib --features fake-model network::tests::a_proxy_uses_no_upstream_proxy_and_no_socks"
    "exec-network-recheck|crates/atomic-core/src/codex.rs|            let hosts = crate::network::check_hosts(&hosts)|            let hosts = /*CONTROL_OFF*/ Ok::<Vec<String>, String>(hosts)|--workspace --features ${FEATURES} --test exec_network a_confined_command_reaches_only_the_hosts_it_is_given"
    "exec-network-policy|crates/atomic-core/src/atom.rs|            if let Err(why) = crate::network::check_hosts(&hosts) {|            if let Err(why) = /*CONTROL_OFF*/ Ok::<(), String>(()) { let _ = &hosts;|-p atomic-agentic-core --lib --features fake-model exec_declarations_are_checked_at_load"
    "stop-subreaper|crates/atomic-exec/src/process.rs|            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);|            let _ = libc::PR_SET_CHILD_SUBREAPER; // CONTROL_OFF|-p atomic-agentic-exec --test stop_all stopping_ends_every_process_a_command_started_before_it_returns"
    "stop-wait-complete|crates/atomic-exec/src/process.rs|            if left.is_empty() && complete {|            if left.is_empty() /*CONTROL_OFF*/ {|-p atomic-agentic-exec --lib the_wait_ends_only_after_a_complete_look"
    "stop-keep-found|crates/atomic-exec/src/process.rs|                found.insert(process);|                let _ = &process; // CONTROL_OFF|-p atomic-agentic-exec --lib the_stop_phase_is_bounded_and_keeps_what_it_found"
    "stop-kill-on-sight|crates/atomic-exec/src/process.rs|                if watched.insert(process) {|                if /*CONTROL_OFF*/ false && watched.insert(process) {|-p atomic-agentic-exec --test stop_all stopping_ends_every_process_a_command_started_before_it_returns"
    "stop-own-tool|crates/atomic-exec/src/process.rs|                let own = own(&all, &tools);|                let own = own(&all, &[] /*CONTROL_OFF*/);|-p atomic-agentic-exec --test stop_all stopping_ends_every_process_a_command_started_before_it_returns"
    "stop-look-cap|crates/atomic-exec/src/process.rs|            look.saturating_mul(2).min(LOOK_MAX)|            look.saturating_mul(2) /*CONTROL_OFF*/|-p atomic-agentic-exec --lib the_next_look_grows_only_when_one_ran_out_and_is_capped"
    "exec-caches-own|crates/atomic-core/src/runtime.rs|    env.extend(declared_env(atom));|    writable_roots.extend(caches.map(crate::worktree::Caches::dir).and_then(std::path::Path::parent).map(std::path::Path::to_path_buf)); env.extend(declared_env(atom));|--workspace --features ${FEATURES} --test exec_caches an_exec_steps_caches_are_its_runs_alone_and_go_with_it"
    "signin-personal-only|crates/atomic/src/engine.rs|            (Some(why), true) => {|            (Some(why), true) if std::hint::black_box(false) => {|-p atomic-agentic --test model_auth -- a_shared_store_or_serve_refuses_the_personal_sign_in where_the_sign_in_is_refused_nothing_takes_its_place_and_it_says_what_to_do"
    "process-hardening|crates/atomic-core/src/codex.rs|        codex_process_hardening::pre_main_hardening();|        let _ = codex_process_hardening::pre_main_hardening;|-p atomic-agentic --test model_auth a_running_atomic_cannot_be_read_through_proc_by_its_own_user"
    "signin-restrictions|crates/atomic-core/src/codex.rs|        sign_in.allowed_by(&config.auth_config())?;|        let _ = sign_in;|-p atomic-agentic-core --lib --features fake-model codex_login_restrictions_refuse_a_sign_in_they_do_not_allow"
    "signin-logout-both-stores|crates/atomic-core/src/signin.rs|            AuthCredentialsStoreMode::File,|            AuthCredentialsStoreMode::Keyring,|-p atomic-agentic --test model_auth logout_forgets_the_file_copy_whichever_store_is_chosen"
    "signin-email-masked-text|crates/atomic/src/main.rs|                if let Some(email) = a.email.as_deref().map(atomic_core::signin::mask_email) {|                if let Some(email) = a.email.as_deref() {|-p atomic-agentic --test model_auth login_status_says_who_is_signed_in_and_logout_forgets_it"
    "signin-email-masked-json|crates/atomic/src/main.rs|                    \"email\": a.email.as_deref().map(atomic_core::signin::mask_email),|                    \"email\": a.email.as_deref(),|-p atomic-agentic --test model_auth login_status_says_who_is_signed_in_and_logout_forgets_it"
    "signin-token-overrides|crates/atomic-core/src/codex.rs|        remove(name);|        let _ = name;|-p atomic-agentic-core --lib the_environment_cannot_stand_in_for_or_redirect_a_sign_ins_tokens"
    "codex-home-read-only|crates/atomic-core/src/codex.rs|    for home in homes.into_iter().flatten() {|    for home in homes.into_iter().flatten().take(0) {|-p atomic-agentic-core --lib private_state_is_denied_and_the_rest_of_the_state_directory_is_not"
    "codex-home-files|crates/atomic-core/src/codex.rs|        planted_never(&codex_home.join(name))?;|        let _ = name;|-p atomic-agentic-core --lib private_state_is_denied_and_the_rest_of_the_state_directory_is_not"
    "review-escalates|crates/atomic/src/pipeline.rs|            _ => break (verdict, said),|            atomic_core::workflow::Verdict::Changes => return Ok((candidate.clone(), review_candidate(&candidate))), _ => break (verdict, said),|-p atomic-agentic --test pipeline a_reviewer_catches_a_seeded_bug_and_escalates_after_its_repair_rounds"
    "artifact-hold-gather|crates/atomic-core/src/artifact/files.rs|            held.push(store.hold(&s.artifact()?)?);|            let _ = &mut held; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model -- a_sweep_racing_a_slow_gather_keeps_the_gathered_files a_store_held_while_files_are_stored_leaves_the_outcome_unknown"
    "artifact-hold-sweep|crates/atomic-core/src/artifact.rs|        let (held, gone) = holds(self.held_dir(), self.owners(), &dead, STAGING_GRACE, now)?;|        let (_held, gone) = holds(self.held_dir(), self.owners(), &dead, STAGING_GRACE, now)?; let held = std::collections::HashSet::<String>::new(); // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model -- a_sweep_racing_a_slow_gather_keeps_the_gathered_files a_hold_keeps_content_until_it_is_let_go_or_its_store_is_gone"
    "artifact-hold-dead|crates/atomic-core/src/artifact.rs|            gone.push(path);|            let _ = path; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model -- a_hold_keeps_content_until_it_is_let_go_or_its_store_is_gone opening_a_store_sweeps_only_dead_stores_leftovers"
    "artifact-hold-release|crates/atomic/src/pipeline.rs|    drop(release);|    std::mem::forget(release); // CONTROL_OFF|-p atomic-agentic --test pipeline an_exec_steps_files_are_let_go_once_its_turn_is_recorded"
    "artifact-hold-release-on-drop|crates/atomic-core/src/runtime.rs|        self.artifacts.release(&self.identity);|        let _ = &self.identity; // CONTROL_OFF|-p atomic-agentic-core --lib --features fake-model a_store_held_while_files_are_stored_leaves_the_outcome_unknown"
    "artifact-hold-snapshot|crates/atomic/src/retention.rs|    artifacts.extend(held);|    let _ = held; // CONTROL_OFF|-p atomic-agentic --test retention a_hold_let_go_while_a_sweep_reads_the_runs_still_keeps_its_content"
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

# Put a file's saved original back, and make its mtime now. A plain cp already
# does that, so cargo rebuilds what it built with the control off; the touch
# keeps that true for a cp that preserves timestamps (cp -p, or an alias).
put_back() {
    cp -- "$(copy_of "$1")" "$1" && touch -- "$1"
}

# git's answer for how a file stands against HEAD: 0 when it matches the
# commit, 1 when it differs, and anything else when git could not be asked --
# in a linked worktree whose main checkout's .git is not mounted, for instance.
# A file git does not track, or one marked assume-unchanged or skip-worktree,
# counts as differing: git diff would say nothing about it either way.
head_diff() {
    local code=0 listed
    listed="$(git ls-files -v -- "$1")" || return 2
    case "${listed}" in
        "H "*) ;;
        *) return 1 ;;
    esac
    git diff --quiet HEAD -- "$1" || code=$?
    return "${code}"
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

# Refuse to start on a file that already differs from HEAD. The run saves what
# it finds and puts that back, so a file dirty before the start would end the
# run dirty again and read as a clean restore; and a switch applied to someone
# else's uncommitted change is not this control. HEAD is the baseline for the
# whole run, so ask for it before anything is edited.
dirty=" "
seen=" "
for row in "${selected[@]}"; do
    IFS='|' read -r _ file _ <<<"${row}"
    case "${seen}" in *" ${file} "*) continue ;; esac
    code=0
    head_diff "${file}" || code=$?
    if [ "${code}" -eq 1 ]; then
        dirty+="${file} "
    elif [ "${code}" -ne 0 ]; then
        die_unedited "cannot compare ${file} with HEAD"
    fi
    seen+="${file} "
done
[ "${dirty}" = " " ] ||
    die_unedited "these files differ from HEAD:${dirty% *} (a file git does not track as committed counts). A run edits them and puts back what it found, so it would restore that state and read as a clean restore; commit them or put them back first"

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
# named, and the originals stay. The saved copies and the manifest are this
# script's own state, so neither is asked what the source should be: after the
# hash, git says whether the file matches HEAD, and a restore that leaves it
# differing is a failure even when it matches the copy. Each file is touched on
# the way back, so the next cargo run rebuilds it.
restore() {
    local bad=0 hash file code
    if [ ! -r "${STATE}/manifest" ]; then
        echo "scripts/controls-check.sh: ${STATE}/manifest cannot be read, so no file can be checked. Compare the files with git and ${STATE} by hand." >&2
        return 1
    fi
    while read -r hash file; do
        cp -- "$(copy_of "${file}")" "${file}" 2>/dev/null
        if [ "$(git hash-object -- "${file}" 2>/dev/null)" != "${hash}" ]; then
            echo "scripts/controls-check.sh: ${file} IS LEFT MODIFIED: it does not match its original, ${hash}. Put it back by hand; ${STATE} keeps what was saved." >&2
            bad=1
            continue
        fi
        code=0
        head_diff "${file}" || code=$?
        if [ "${code}" -ne 0 ]; then
            if [ "${code}" -eq 1 ]; then
                echo "scripts/controls-check.sh: ${file} IS LEFT DIFFERENT FROM HEAD: it matches the copy made at the start, which was not HEAD. Put it back from HEAD; ${STATE} keeps what was saved." >&2
            else
                echo "scripts/controls-check.sh: cannot compare ${file} with HEAD, so it cannot be shown to be back as committed." >&2
            fi
            bad=1
            continue
        fi
        touch -- "${file}" 2>/dev/null
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
    # Every test binary runs, however an earlier one ends: a row may name
    # tests in several, and each must be seen to fail.
    cargo test --locked --no-fail-fast ${1} >"${LOG}" 2>&1 &
    cargo_pid=$!
    wait "${cargo_pid}"
    local code=$?
    cargo_pid=""
    return "${code}"
}
# The tests a row's cargo arguments name: those after `--`, or else its
# positional filters.
named_tests() {
    local after=0 skip=0 a
    # shellcheck disable=SC2086
    for a in ${1}; do
        if [ "${skip}" -eq 1 ]; then
            skip=0
            continue
        fi
        # After `--`, the test binary's own flags, such as `--nocapture`, are
        # not tests.
        if [ "${after}" -eq 1 ]; then
            case "${a}" in
            -*) ;;
            *) echo "${a}" ;;
            esac
            continue
        fi
        case "${a}" in
        --) after=1 ;;
        -p | --package | --features | --test | --bin | --example) skip=1 ;;
        -*) ;;
        *) echo "${a}" ;;
        esac
    done
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
    put_back "${file}" || die "cannot put ${file} back"
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
    # And each one it names is seen failing: one that matched no test, or
    # was never run, is not a test that caught the control's removal.
    unfailed=""
    while read -r test; do
        # The name as it is, not as a pattern: a failed test's path holds it.
        grep -E '^test [^ ]+ \.\.\. FAILED$' "${LOG}" | cut -d' ' -f2 | grep -qF -- "${test}" ||
            unfailed="${unfailed} ${test}"
    done < <(named_tests "${args}")
    if [ -n "${unfailed}" ]; then
        echo "FAIL ${name}: with the control off, these tests were not seen failing:${unfailed}" >&2
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
