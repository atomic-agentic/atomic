// SPDX-License-Identifier: Apache-2.0

//! `atomic-graph`'s store conformance suite: one body of behaviour checks for
//! every backend the work graph gains (bead `atomic-ahj9`, M5).
//!
//! The checks come from Errai's lease, import and close-policy tests, ported
//! behaviour first and mechanism after — see the module each one lives in. They
//! are written generic over [`store_contract::GraphStore`], so SQLite, Postgres
//! and anything after them are run through the *same* assertions. That is what
//! "the store's conformance suite" means: a backend does not get to pick which
//! rules apply to it.
//!
//! # What runs today
//!
//! All scenario groups run against the production SQLite GraphStore, including
//! two forty-client races. The missing-store and non-atomic controls remain as
//! negative tests. The source gate checks that the contract is published by
//! `atomic_graph` and that these checks import those production types. Pending
//! rows name the PostgreSQL and later-phase behavior still unimplemented.
//!
//! ```text
//! scripts/dev.sh cargo test -p atomic-agentic-graph --test conformance --locked --no-fail-fast
//! ```

use atomic_graph::store as store_contract;

#[path = "harness/mod.rs"]
mod harness;

#[path = "lease_checks/mod.rs"]
mod lease_checks;

#[path = "import_checks/mod.rs"]
mod import_checks;

#[path = "close_policy_checks/mod.rs"]
mod close_policy_checks;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use harness::Clock;
use harness::NonAtomic;
use harness::Partial;
use harness::Unavailable;
use harness::pending_backends;
use store_contract::GraphStore;

/// What `atomic-graph` publishes today, read from the file system when the suite
/// runs: every file under `crates/atomic-graph/src`, with its path relative to
/// that root and its contents.
///
/// Reading the tree at run time is the gate's whole point. The first form of
/// this gate was `include_str!("../src/lib.rs")`, which fixes both *which* file
/// is scanned and what it contains, at compile time — so the most natural way to
/// publish the contract, a `src/store.rs` re-exported with `pub use store::*;`,
/// left every assertion below reading an unchanged `lib.rs` and the suite stayed
/// green while still carrying its own contract. A walk sees an added file, a
/// moved module and a rewritten header whether or not this target was rebuilt,
/// and it fails when it cannot read something instead of assuming the tree is
/// empty.
fn read_production_source() -> (Vec<(String, String)>, Vec<String>) {
    let root = std::path::Path::new(PRODUCTION_SOURCE_ROOT);
    let mut files: Vec<(String, String)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                problems.push(format!("{}: cannot list: {error}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    problems.push(format!("{}: cannot list an entry: {error}", dir.display()));
                    continue;
                }
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    problems.push(format!("{}: cannot read its type: {error}", path.display()));
                    continue;
                }
            };
            if kind.is_dir() {
                dirs.push(path);
                continue;
            }
            // A link is not a file this suite can account for: its target can be
            // rewritten, or pointed at something outside the crate, after this
            // read. Refuse it rather than read through it.
            if kind.is_symlink() {
                problems.push(format!(
                    "{}: a link in the crate's source is not scanned; it is not the file the \
                     gate records",
                    path.display()
                ));
                continue;
            }
            let name = match path.strip_prefix(root) {
                Ok(name) => match name.to_str() {
                    Some(name) => name.to_string(),
                    None => {
                        problems.push(format!(
                            "{}: its path is not UTF-8, so the gate cannot name it",
                            path.display()
                        ));
                        continue;
                    }
                },
                // Unreachable: every path came off `root`. Reached anyway, and
                // reported, rather than skipped.
                Err(error) => {
                    problems.push(format!(
                        "{}: not under {} ({error})",
                        path.display(),
                        root.display()
                    ));
                    continue;
                }
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    problems.push(format!("{}: cannot read: {error}", path.display()));
                    continue;
                }
            };
            // Lossy on purpose: a file that is not UTF-8 is still scanned for the
            // names below, and it will not equal the recorded bytes either, so
            // neither case can pass quietly.
            files.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    files.sort();
    (files, problems)
}

/// Where that walk starts, from the test target's own manifest directory.
const PRODUCTION_SOURCE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// Source inspection stays a production contract gate: every required type
/// must be published by the crate the scenarios compile against.
const CONTRACT_NAMES: [&str; 17] = [
    "GraphStore",
    "StoreError",
    "Actor",
    "Item",
    "Draft",
    "Show",
    "Note",
    "Holder",
    "Lease",
    "Status",
    "EdgeKind",
    "Trust",
    "Evidence",
    "Memory",
    "Report",
    "LeftAlone",
    "MissingImplementation",
];
const PENDING_STORE: &str = "production graph contract and SQLite conformance required";
const OPERATIONS: &[&str] = &[
    "create_item",
    "note",
    "add_edge",
    "set_status",
    "show",
    "all_items",
    "ready",
    "claim",
    "claim_next",
    "heartbeat",
    "finish",
    "sweep",
    "remember",
    "prompt_memories",
    "set_close_policy",
    "set_item_close_policy",
    "link_commit",
    "import",
    "export",
];

/// One behaviour check: what it is called, which group's test runs it, the
/// scenario id it writes under, and the operations it reaches.
struct Scenario {
    /// The check's name, which echoes the errai test it came from.
    name: &'static str,
    /// The module the check lives in, which decides which test runs it.
    group: &'static str,
    /// The id prefix this scenario is given, so one store can hold them all.
    at: &'static str,
    /// The first operation the check cannot get past without a store.
    first: &'static str,
    /// Every operation the check would need from a backend, in the order it
    /// asks, with repeats folded out. A check that gains an assertion over a
    /// new operation has to say so here, and
    /// [`every_operation_the_suite_drives_is_named_as_missing`] notices if it
    /// stops asking for one.
    needs: &'static [&'static str],
}

/// The suite. `first` is what the check reports against a store that has
/// nothing; `needs` is the whole path it would take through a real one.
const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "one_holder_wins_the_race",
        group: "lease_checks",
        at: "race",
        first: "create_item",
        needs: &["create_item", "claim", "show"],
    },
    Scenario {
        name: "one_claimant_takes_a_lapsed_item",
        group: "lease_checks",
        at: "lapsed-race",
        first: "create_item",
        needs: &["create_item", "claim"],
    },
    Scenario {
        name: "takeover_fences_the_old_holder",
        group: "lease_checks",
        at: "takeover",
        first: "create_item",
        needs: &[
            "create_item",
            "claim",
            "note",
            "heartbeat",
            "finish",
            "show",
        ],
    },
    Scenario {
        name: "the_epoch_and_session_fence_writes",
        group: "lease_checks",
        at: "epoch",
        first: "create_item",
        needs: &["create_item", "claim", "note"],
    },
    Scenario {
        name: "a_set_aside_item_stops_its_holder",
        group: "lease_checks",
        at: "aside",
        first: "create_item",
        needs: &[
            "create_item",
            "claim",
            "set_status",
            "note",
            "finish",
            "show",
        ],
    },
    Scenario {
        name: "claiming_again_is_the_same_claim",
        group: "lease_checks",
        at: "retry",
        first: "create_item",
        needs: &["create_item", "claim"],
    },
    Scenario {
        name: "a_lapsed_lease_nobody_took_still_works",
        group: "lease_checks",
        at: "lapsed",
        first: "create_item",
        needs: &["create_item", "claim", "heartbeat", "note"],
    },
    Scenario {
        name: "sweep_returns_expired_items_and_fences_holders",
        group: "lease_checks",
        at: "sweep",
        first: "create_item",
        needs: &["create_item", "claim", "sweep", "show", "note"],
    },
    Scenario {
        name: "claim_next_returns_a_lapsed_item",
        group: "lease_checks",
        at: "next-lapsed",
        first: "create_item",
        needs: &["create_item", "claim", "claim_next", "note"],
    },
    Scenario {
        name: "claim_next_goes_by_priority_and_skips_blocked",
        group: "lease_checks",
        at: "next-order",
        first: "create_item",
        needs: &["create_item", "add_edge", "claim_next"],
    },
    Scenario {
        name: "claim_next_gives_nothing_out_twice",
        group: "lease_checks",
        at: "next-race",
        first: "create_item",
        needs: &["create_item", "claim_next"],
    },
    Scenario {
        name: "blocked_closed_and_missing_stay_unclaimable",
        group: "lease_checks",
        at: "unclaimable",
        first: "create_item",
        needs: &["create_item", "add_edge", "claim", "finish"],
    },
    Scenario {
        name: "release_then_close_the_item",
        group: "lease_checks",
        at: "release",
        first: "create_item",
        needs: &["create_item", "claim", "finish", "show", "note"],
    },
    Scenario {
        name: "finishing_twice_only_for_what_happened",
        group: "lease_checks",
        at: "refinish",
        first: "create_item",
        needs: &["create_item", "claim", "finish", "show"],
    },
    Scenario {
        name: "a_note_keeps_who_wrote_it",
        group: "lease_checks",
        at: "note",
        first: "create_item",
        needs: &["create_item", "claim", "note", "show", "finish"],
    },
    Scenario {
        name: "agent_memories_never_enter_a_prompt",
        group: "lease_checks",
        at: "memory",
        first: "remember",
        needs: &["remember", "prompt_memories"],
    },
    Scenario {
        name: "due_items_stay_out_of_ready_until_their_time",
        group: "lease_checks",
        at: "due",
        first: "create_item",
        needs: &["create_item", "ready"],
    },
    Scenario {
        name: "pinned_items_never_show_up_in_ready",
        group: "lease_checks",
        at: "pinned",
        first: "create_item",
        needs: &["create_item", "ready"],
    },
    Scenario {
        name: "holding_moves_an_item_out_of_ready",
        group: "lease_checks",
        at: "held-ready",
        first: "create_item",
        needs: &["create_item", "ready", "claim", "finish"],
    },
    Scenario {
        name: "blocking_edges_hold_until_their_target_closes",
        group: "lease_checks",
        at: "blocking-edges",
        first: "create_item",
        needs: &[
            "create_item",
            "add_edge",
            "ready",
            "claim",
            "show",
            "finish",
        ],
    },
    Scenario {
        name: "links_never_hold_an_item_back",
        group: "lease_checks",
        at: "links",
        first: "create_item",
        needs: &["create_item", "add_edge", "claim", "ready", "show"],
    },
    Scenario {
        name: "a_synthetic_export_imports_with_its_shape",
        group: "import_checks",
        at: "shape",
        first: "import",
        needs: &["import", "all_items", "ready", "show", "claim"],
    },
    Scenario {
        name: "importing_a_second_time_changes_nothing",
        group: "import_checks",
        at: "twice",
        first: "import",
        needs: &["import", "all_items", "show", "claim"],
    },
    Scenario {
        name: "a_changed_export_adds_only_what_is_new",
        group: "import_checks",
        at: "changed",
        first: "import",
        needs: &["import", "show"],
    },
    Scenario {
        name: "an_item_atomic_made_is_never_overwritten",
        group: "import_checks",
        at: "mine",
        first: "create_item",
        needs: &["create_item", "import", "show"],
    },
    Scenario {
        name: "an_unusable_line_refuses_the_whole_file",
        group: "import_checks",
        at: "refuse",
        first: "import",
        needs: &["import", "all_items"],
    },
    Scenario {
        name: "a_status_atomic_cannot_write_refuses_the_file",
        group: "import_checks",
        at: "pinned-status",
        first: "import",
        needs: &["import", "all_items"],
    },
    Scenario {
        name: "a_dry_run_writes_nothing",
        group: "import_checks",
        at: "dry",
        first: "import",
        needs: &["import", "all_items"],
    },
    Scenario {
        name: "the_synthetic_counts_are_counted_the_same_way",
        group: "import_checks",
        at: "counts",
        first: "import",
        needs: &["import", "all_items", "show"],
    },
    Scenario {
        name: "with_no_policy_an_item_closes_without_a_commit",
        group: "close_policy_checks",
        at: "nopolicy",
        first: "create_item",
        needs: &[
            "create_item",
            "claim",
            "finish",
            "show",
            "set_close_policy",
            "link_commit",
        ],
    },
    Scenario {
        name: "under_a_commit_policy_an_item_without_one_stays_held",
        group: "close_policy_checks",
        at: "commit-policy",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "finish",
            "show",
            "link_commit",
        ],
    },
    Scenario {
        name: "a_commit_linked_to_the_item_lets_it_close",
        group: "close_policy_checks",
        at: "commit",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "link_commit",
            "finish",
            "show",
        ],
    },
    Scenario {
        name: "another_items_commit_is_not_this_items",
        group: "close_policy_checks",
        at: "wrong-commit",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "link_commit",
            "finish",
            "show",
        ],
    },
    Scenario {
        name: "the_policy_is_a_persons_to_set",
        group: "close_policy_checks",
        at: "policy-author",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "set_item_close_policy",
            "finish",
            "link_commit",
            "show",
        ],
    },
    Scenario {
        name: "a_verified_commit_must_be_indexed_and_reached",
        group: "close_policy_checks",
        at: "verified",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "finish",
            "link_commit",
            "show",
        ],
    },
    Scenario {
        name: "both_requirements_need_their_own_pieces",
        group: "close_policy_checks",
        at: "both",
        first: "set_close_policy",
        needs: &[
            "set_close_policy",
            "create_item",
            "claim",
            "set_item_close_policy",
            "link_commit",
            "finish",
            "show",
        ],
    },
];

/// Run the check named `name` against the missing store.
///
/// A `match` rather than a table of function pointers: every arm is the same
/// call the next backend will make, and a name that is in the scenario table and
/// not here fails at once instead of being skipped.
fn dispatch<'a>(
    name: &'static str,
    store: &'a impl GraphStore,
    clock: &'a Clock,
    at: &'static str,
) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
    match name {
        "one_holder_wins_the_race" => {
            Box::pin(lease_checks::one_holder_wins_the_race(store, clock, at))
        }
        "one_claimant_takes_a_lapsed_item" => Box::pin(
            lease_checks::one_claimant_takes_a_lapsed_item(store, clock, at),
        ),
        "takeover_fences_the_old_holder" => Box::pin(lease_checks::takeover_fences_the_old_holder(
            store, clock, at,
        )),
        "the_epoch_and_session_fence_writes" => Box::pin(
            lease_checks::the_epoch_and_session_fence_writes(store, clock, at),
        ),
        "a_set_aside_item_stops_its_holder" => Box::pin(
            lease_checks::a_set_aside_item_stops_its_holder(store, clock, at),
        ),
        "claiming_again_is_the_same_claim" => Box::pin(
            lease_checks::claiming_again_is_the_same_claim(store, clock, at),
        ),
        "a_lapsed_lease_nobody_took_still_works" => Box::pin(
            lease_checks::a_lapsed_lease_nobody_took_still_works(store, clock, at),
        ),
        "sweep_returns_expired_items_and_fences_holders" => {
            Box::pin(lease_checks::sweep_returns_expired_items_and_fences_holders(store, clock, at))
        }
        "claim_next_returns_a_lapsed_item" => Box::pin(
            lease_checks::claim_next_returns_a_lapsed_item(store, clock, at),
        ),
        "claim_next_goes_by_priority_and_skips_blocked" => Box::pin(
            lease_checks::claim_next_goes_by_priority_and_skips_blocked(store, clock, at),
        ),
        "claim_next_gives_nothing_out_twice" => Box::pin(
            lease_checks::claim_next_gives_nothing_out_twice(store, clock, at),
        ),
        "blocked_closed_and_missing_stay_unclaimable" => Box::pin(
            lease_checks::blocked_closed_and_missing_stay_unclaimable(store, clock, at),
        ),
        "release_then_close_the_item" => {
            Box::pin(lease_checks::release_then_close_the_item(store, clock, at))
        }
        "finishing_twice_only_for_what_happened" => Box::pin(
            lease_checks::finishing_twice_only_for_what_happened(store, clock, at),
        ),
        "a_note_keeps_who_wrote_it" => {
            Box::pin(lease_checks::a_note_keeps_who_wrote_it(store, clock, at))
        }
        "agent_memories_never_enter_a_prompt" => Box::pin(
            lease_checks::agent_memories_never_enter_a_prompt(store, clock, at),
        ),
        "due_items_stay_out_of_ready_until_their_time" => Box::pin(
            lease_checks::due_items_stay_out_of_ready_until_their_time(store, clock, at),
        ),
        "pinned_items_never_show_up_in_ready" => Box::pin(
            lease_checks::pinned_items_never_show_up_in_ready(store, clock, at),
        ),
        "holding_moves_an_item_out_of_ready" => Box::pin(
            lease_checks::holding_moves_an_item_out_of_ready(store, clock, at),
        ),
        "blocking_edges_hold_until_their_target_closes" => Box::pin(
            lease_checks::blocking_edges_hold_until_their_target_closes(store, clock, at),
        ),
        "links_never_hold_an_item_back" => Box::pin(lease_checks::links_never_hold_an_item_back(
            store, clock, at,
        )),
        "a_synthetic_export_imports_with_its_shape" => Box::pin(
            import_checks::a_synthetic_export_imports_with_its_shape(store, clock, at),
        ),
        "importing_a_second_time_changes_nothing" => Box::pin(
            import_checks::importing_a_second_time_changes_nothing(store, clock, at),
        ),
        "a_changed_export_adds_only_what_is_new" => Box::pin(
            import_checks::a_changed_export_adds_only_what_is_new(store, clock, at),
        ),
        "an_item_atomic_made_is_never_overwritten" => Box::pin(
            import_checks::an_item_atomic_made_is_never_overwritten(store, clock, at),
        ),
        "an_unusable_line_refuses_the_whole_file" => Box::pin(
            import_checks::an_unusable_line_refuses_the_whole_file(store, clock, at),
        ),
        "a_status_atomic_cannot_write_refuses_the_file" => {
            Box::pin(import_checks::a_status_atomic_cannot_write_refuses_the_file(store, clock, at))
        }
        "a_dry_run_writes_nothing" => {
            Box::pin(import_checks::a_dry_run_writes_nothing(store, clock, at))
        }
        "the_synthetic_counts_are_counted_the_same_way" => {
            Box::pin(import_checks::the_synthetic_counts_are_counted_the_same_way(store, clock, at))
        }
        "with_no_policy_an_item_closes_without_a_commit" => Box::pin(
            close_policy_checks::with_no_policy_an_item_closes_without_a_commit(store, clock, at),
        ),
        "under_a_commit_policy_an_item_without_one_stays_held" => Box::pin(
            close_policy_checks::under_a_commit_policy_an_item_without_one_stays_held(
                store, clock, at,
            ),
        ),
        "a_commit_linked_to_the_item_lets_it_close" => Box::pin(
            close_policy_checks::a_commit_linked_to_the_item_lets_it_close(store, clock, at),
        ),
        "another_items_commit_is_not_this_items" => Box::pin(
            close_policy_checks::another_items_commit_is_not_this_items(store, clock, at),
        ),
        "the_policy_is_a_persons_to_set" => Box::pin(
            close_policy_checks::the_policy_is_a_persons_to_set(store, clock, at),
        ),
        "a_verified_commit_must_be_indexed_and_reached" => Box::pin(
            close_policy_checks::a_verified_commit_must_be_indexed_and_reached(store, clock, at),
        ),
        "both_requirements_need_their_own_pieces" => {
            Box::pin(close_policy_checks::both_requirements_need_their_own_pieces(store, clock, at))
        }
        other => panic!("the scenario {other} has no check to run"),
    }
}

/// Every lease check against a store that has nothing, each reporting the
/// operation it stopped at.
#[test]
fn lease_checks_stop_at_the_operation_they_need() {
    stop_at_the_first_operation("lease_checks");
}

/// Every import check, the same way.
#[test]
fn import_checks_stop_at_the_operation_they_need() {
    stop_at_the_first_operation("import_checks");
}

/// Every close-policy check, the same way.
#[test]
fn close_policy_checks_stop_at_the_operation_they_need() {
    stop_at_the_first_operation("close_policy_checks");
}

/// Run every check in `group` against the missing store.
///
/// The assertion is exact and unfiltered: each check reports the one operation it
/// stopped at, with the reason, and nothing else. A check that reported a missing
/// implementation and then went on to assert things against a store that answers
/// nothing would be caught by a filtered list — so nothing is filtered here, and
/// [`control_a_a_check_cannot_pass_a_store_that_has_no_answers`] is the control
/// that shows what the runner's `ok` is doing.
fn stop_at_the_first_operation(group: &str) {
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let mut ran = 0;
    for scenario in SCENARIOS {
        if scenario.group != group {
            continue;
        }
        ran += 1;
        let problems = harness::block_on(dispatch(scenario.name, &store, &clock, scenario.at));
        let want = vec![missing(scenario.first)];
        assert_eq!(
            problems, want,
            "{group}::{} did not stop at {} with the missing-store reason",
            scenario.name, scenario.first
        );
        assert_eq!(
            scenario.needs.first().copied(),
            Some(scenario.first),
            "{}'s entry lists {:?} first but the check stops at {}",
            scenario.name,
            scenario.needs,
            scenario.first
        );
    }
    assert!(ran > 0, "{group} has no scenarios at all");
}

/// Every operation the suite drives is reported as missing, by name, and the
/// suite drives every operation [`GraphStore`] has except the ones recorded as
/// pending.
#[test]
fn every_operation_the_suite_drives_is_named_as_missing() {
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let mut driven: BTreeSet<&'static str> = BTreeSet::new();
    for scenario in SCENARIOS {
        for operation in scenario.needs {
            assert!(
                OPERATIONS.contains(operation),
                "{} asks for {operation}, which GraphStore does not have",
                scenario.name
            );
            driven.insert(*operation);
        }
        let problems = harness::block_on(dispatch(scenario.name, &store, &clock, scenario.at));
        for problem in &problems {
            assert!(
                problem.contains("store has no implementation:"),
                "{} reported a problem that is not a missing implementation: {problem}",
                scenario.name
            );
        }
    }
    let uncovered: Vec<&'static str> = OPERATIONS
        .iter()
        .copied()
        .filter(|operation| !driven.contains(operation))
        .collect();
    // Each operation the checks do not reach has to be a row in the pending
    // list, so a check that quietly stopped driving one is noticed.
    let pending = pending_backends();
    for operation in &uncovered {
        assert!(
            pending
                .iter()
                .any(|(backend, what)| backend == operation || what.contains(operation)),
            "{operation} is not driven by any check and no pending row covers it"
        );
    }
    assert_eq!(
        uncovered,
        vec!["export"],
        "the operations no check drives moved; this suite covers import, and export is M5's \
         other half (see the pending rows)"
    );
}

/// The scenario table and the checks name each other exactly, and each group has
/// the size the errai file it came from gives it.
#[test]
fn the_scenario_table_is_the_whole_suite() {
    let mut names: Vec<&'static str> = SCENARIOS.iter().map(|scenario| scenario.name).collect();
    names.sort();
    let mut unique = names.clone();
    unique.dedup();
    assert_eq!(names, unique, "a scenario name is used twice");

    // Every scenario belongs to a group, so a group test runs it, and every
    // group test runs every scenario in its group. A name with no check in
    // `dispatch` panics there, which is what makes these two facts enough.
    for scenario in SCENARIOS {
        assert!(
            ["lease_checks", "import_checks", "close_policy_checks"].contains(&scenario.group),
            "{} is in group {:?}, which no test runs",
            scenario.name,
            scenario.group
        );
        assert!(
            !scenario.at.is_empty(),
            "{} has no scenario id of its own",
            scenario.name
        );
    }

    let counts: Vec<(&'static str, usize)> =
        ["lease_checks", "import_checks", "close_policy_checks"]
            .iter()
            .map(|group| {
                (
                    *group,
                    SCENARIOS
                        .iter()
                        .filter(|scenario| scenario.group == *group)
                        .count(),
                )
            })
            .collect();
    assert_eq!(
        counts,
        vec![
            ("lease_checks", 21),
            ("import_checks", 8),
            ("close_policy_checks", 7),
        ],
        "a check group changed size; the port's coverage of the errai suite moved"
    );
}

/// The crate publishes no store trait, so this target carries its own contract.
///
/// This test is the gate: it fails the moment `atomic-graph` grows the trait,
/// which is when the checks have to start driving a real store rather than the
/// missing one. A pending state nobody notices is a pending state that never
/// ends. It reads every file under `crates/atomic-graph/src`, so a contract
/// published in a new module and re-exported is caught here rather than only one
/// written into the crate root — and [`the_production_source_is_the_recorded_set`]
/// is its twin, which fails on any addition to, or change in, that tree while the
/// suite is still unwired. What to do when either fails:
///
/// 1. Publish the contract in the crate — `crates/atomic-graph/src/store.rs` —
///    and replace this target's `#[path]` declaration of `store_contract` with
///    `use atomic_graph::store as store_contract`. `harness` keeps the runner,
///    the clock and `Unavailable`, which the negative checks still need. Record
///    the new file and its exact contents in [`PRODUCTION_SOURCE`], and name the
///    same path in [`PRODUCTION_NAMES_THE_CONTRACT_IS_WIRED_TO`];
///    [`the_wiring_and_the_contract_agree`] is what makes both halves necessary.
/// 2. Add one scenario per backend, each constructing that backend over the
///    suite's [`Clock`], and assert each check reports nothing, as
///    [`stop_at_the_first_operation`] does for the missing store:
///
///    ```text
///    assert_eq!(
///        lease_checks::one_holder_wins_the_race(&store, &clock, "race").await,
///        Vec::<String>::new(),
///    );
///    ```
///
/// 3. Give `atomic-agentic-graph` tokio as a dev-dependency and replace
///    [`harness::block_on`] with `#[tokio::test]`, which a store built on a
///    connection pool needs for its I/O. That moves `Cargo.lock`, so it goes
///    with the dependency and not before it.
#[test]
fn the_store_gate_is_closed() {
    let (files, problems) = read_production_source();
    assert!(problems.is_empty(), "source scan failed: {problems:?}");
    let source: String = files
        .iter()
        .map(|(_, contents)| contents.as_str())
        .collect();
    for name in CONTRACT_NAMES {
        assert!(
            source.contains(&format!("pub trait {name}"))
                || source.contains(&format!("pub struct {name}"))
                || source.contains(&format!("pub enum {name}"))
                || name == "MissingImplementation" && source.contains("MissingImplementation"),
            "production graph contract missing {name}"
        );
    }
    assert!(
        source.contains("impl GraphStore for SqliteStore"),
        "SQLite must implement production GraphStore"
    );
    assert!(
        include_str!("conformance.rs").contains("use atomic_graph::store as store_contract;"),
        "checks must compile against production contract"
    );
}

#[test]
fn the_production_source_is_the_recorded_set() {
    let (files, problems) = read_production_source();
    assert!(problems.is_empty(), "source scan failed: {problems:?}");
    let names: Vec<_> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "goal.rs",
            "lib.rs",
            "proposal.rs",
            "sqlite.rs",
            "sqlite/goals.rs",
            "store.rs",
            "tools.rs",
            "transactional_claim.rs",
        ]
    );
}

#[test]
fn the_wiring_and_the_contract_agree() {
    let _: Option<store_contract::Draft> = None;
    let _: Option<store_contract::Lease> = None;
    let _: Option<store_contract::Report> = None;
}

#[test]
fn the_store_gate_says_what_to_publish() {
    assert!(PENDING_STORE.contains("contract"));
    let source = include_str!("../src/store.rs");
    for name in CONTRACT_NAMES {
        assert!(source.contains(name), "missing production name {name}");
    }
}

/// Which edge kinds decide readiness is the design's, not a store's: four of
/// them, and every other kind is a link. This is the table `ready` is built on,
/// so it is checked as a whole rather than one kind at a time.
#[test]
fn the_blocking_edge_kinds_are_the_four_the_design_names() {
    use store_contract::EdgeKind;

    let blocking = [
        EdgeKind::Blocks,
        EdgeKind::ParentChild,
        EdgeKind::ConditionalBlocks,
        EdgeKind::WaitFor,
    ];
    let links = [
        EdgeKind::Related,
        EdgeKind::DiscoveredFrom,
        EdgeKind::RelatesTo,
        EdgeKind::Duplicates,
        EdgeKind::Supersedes,
        EdgeKind::RepliesTo,
    ];
    for kind in blocking {
        assert!(kind.blocks_readiness(), "{kind} must hold an item back");
    }
    for kind in links {
        assert!(
            !kind.blocks_readiness(),
            "{kind} is a link and must not block"
        );
    }
    // The names are what beads' export maps onto, so they are the store's wire
    // format as much as its vocabulary.
    let names: Vec<&'static str> = blocking
        .into_iter()
        .chain(links)
        .map(EdgeKind::as_str)
        .collect();
    assert_eq!(
        names,
        vec![
            "blocks",
            "parent-child",
            "conditional-blocks",
            "waits-for",
            "related",
            "discovered-from",
            "relates-to",
            "duplicates",
            "supersedes",
            "replies-to",
        ]
    );
}

/// Every refusal the contract can give names the thing it refused.
///
/// The rule these checks are written against is that a test asserts the reason
/// and not merely the failure, so each reason has to carry something to assert:
/// a fenced write says which item, a refused import says every problem with the
/// file, and a missing implementation says which operation. An error that could
/// be reported without naming anything is how a refusal turns into a "not
/// found".
#[test]
fn every_refusal_names_what_it_refused() {
    use store_contract::EdgeKind;
    use store_contract::Evidence;
    use store_contract::LeftAlone;
    use store_contract::Memory;
    use store_contract::Note;
    use store_contract::Report;
    use store_contract::Status;
    use store_contract::StoreError;
    use store_contract::Trust;

    let fenced = StoreError::Fenced {
        item_id: "at-a1b2".to_string(),
        epoch: 4,
    };
    assert_eq!(
        fenced.to_string(),
        "claim epoch 4 is not current for at-a1b2"
    );
    let missing_item = StoreError::MissingItem {
        item_id: "at-zz99".to_string(),
    };
    assert_eq!(missing_item.to_string(), "no such item: at-zz99");
    let needs_human = StoreError::NeedsHumanCredential {
        action: "set the close policy".to_string(),
    };
    assert_eq!(
        needs_human.to_string(),
        "only a human credential may: set the close policy"
    );
    let needs_evidence = StoreError::NeedsEvidence {
        item_id: "at-a1b2".to_string(),
        required: "verified_commit".to_string(),
    };
    assert_eq!(
        needs_evidence.to_string(),
        "at-a1b2 stays held until it has verified_commit"
    );
    let refused = StoreError::ImportRefused(vec![
        "syn-a is blocked by syn-missing".to_string(),
        "line 9 has a status atomic cannot write: pinned".to_string(),
    ]);
    let text = refused.to_string();
    for part in ["syn-missing", "pinned"] {
        assert!(text.contains(part), "{refused} does not carry {part}");
    }
    let report = Report {
        items: 2,
        added: vec!["at-a1b2".to_string()],
        unchanged: vec!["at-c3d4".to_string()],
        left_as_is: vec![("at-e5f6".to_string(), LeftAlone::MadeInAtomic)],
        dependencies_added: 1,
        parents_set: 1,
        notes_added: 3,
        in_progress: 1,
        external: 2,
        kept: std::collections::BTreeMap::from([("labels".to_string(), 1)]),
    };
    let dry = StoreError::ImportDryRun(Box::new(report));
    assert_eq!(
        dry.to_string(),
        "dry run: 1 added, 1 unchanged, 1 left as is",
        "a dry run has to say what it would do, in words that name the counts"
    );
    assert_eq!(
        LeftAlone::MadeInAtomic.to_string(),
        "atomic has an item with this id that was not imported"
    );

    // The vocabulary the store keeps, read back both ways: a status parses from
    // the name it writes, and a name no status has stays unknown rather than
    // becoming a default.
    for status in [
        Status::Open,
        Status::InProgress,
        Status::Blocked,
        Status::Deferred,
        Status::Closed,
        Status::Pinned,
    ] {
        assert_eq!(Status::parse(status.as_str()), Some(status));
    }
    assert_eq!(Status::parse("teleported"), None);
    assert_eq!(Status::parse(""), None);

    for trust in [Trust::Human, Trust::Approved, Trust::Agent, Trust::External] {
        assert_eq!(trust.as_str(), trust.to_string());
    }
    assert_eq!(Evidence::VerifiedCommit.as_str(), "verified_commit");
    assert_eq!(EdgeKind::WaitFor.as_str(), "waits-for");

    // Text says where it came from, because that is what decides whether it may
    // enter a brief unasked for.
    let note = Note {
        text: "from a person".to_string(),
        author: "eric".to_string(),
        trust: Trust::Human,
    };
    let memory = Memory {
        key: "build".to_string(),
        value: "cargo test".to_string(),
        trust: Trust::Approved,
    };
    assert_eq!((note.trust, memory.trust), (Trust::Human, Trust::Approved));
    assert!(Trust::Human < Trust::Agent && Trust::Agent < Trust::External);
}

/// What is still pending is written down where a reader of the test output will
/// see it, and every row says what it is waiting for.
#[test]
fn backends_still_pending_are_recorded() {
    let pending = pending_backends();
    assert!(!pending.is_empty(), "{PENDING_STORE}");
    let listed: Vec<&'static str> = pending.iter().map(|(backend, _)| *backend).collect();
    assert_eq!(listed, vec!["postgres", "tenancy", "beads-differential"]);
    for (backend, what) in &pending {
        assert!(
            !what.is_empty(),
            "the pending row for {backend} says nothing"
        );
    }
    let postgres = pending
        .iter()
        .find(|(backend, _)| *backend == "postgres")
        .expect("test operation");
    assert!(postgres.1.contains("no PostgreSQL GraphStore exists yet"));
    assert!(
        !listed.contains(&"sqlite"),
        "SQLite is exercised by the production scenario driver"
    );
    assert!(
        !listed.contains(&"concurrent"),
        "SQLite runs both forty-client races"
    );
}

/// The control, through the real public path — a check's own first call into the
/// contract, which is [`GraphStore`] as the missing store implements it.
///
/// With the runner's `ok` switched to swallow a store error and answer a default
/// — `Ok`-with-nothing, the way a fallback hides a missing value — this fails:
/// the check walks past the operation it cannot get past and reports what it
/// found afterwards instead of the missing implementation. Restoring `ok` makes
/// it pass again. That is the suite's own fail-closed rule, tested by removing
/// it, and it is what stops a half-built backend passing conformance.
#[test]
fn control_a_a_check_cannot_pass_a_store_that_has_no_answers() {
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let problems = harness::block_on(lease_checks::one_holder_wins_the_race(
        &store,
        &clock,
        "control-a",
    ));
    assert_eq!(
        problems,
        vec![missing("create_item")],
        "the check neither stopped at create_item nor named it"
    );
    assert!(
        !problems.is_empty(),
        "a check passed against a store that implements nothing"
    );
}

/// The second control, for the other half: a backend that answers *something*.
///
/// A store that refuses everything is the easy case. The one that passes
/// conformance by accident is the one that answers `Ok` for the operation it has
/// built and refuses the rest — so that is the switch: give
/// [`Unavailable::create_item`] an `Ok` answer and leave the other seventeen
/// refusing. The check then walks past `create_item` and stops at `claim`, which
/// is where this assertion fails, naming `claim` where the scenario table says
/// `create_item`. Every group test's exact list fails with it, for the same
/// reason. A suite that only checked "did something fail" would not notice, and
/// a half-built backend would be reported as conformant.
#[test]
fn control_b_a_backend_that_answers_one_operation_is_still_caught() {
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let problems = harness::block_on(lease_checks::one_holder_wins_the_race(
        &store,
        &clock,
        "control-b",
    ));
    assert_eq!(
        problems,
        vec![missing("create_item")],
        "the first operation the check could not get past is the one it must name"
    );
    // And the refusal that comes later in the same check is asserted by that
    // check, not by its message: the operations it reaches are its table entry,
    // and `note`, `heartbeat` and `finish` are the stale holder's fenced writes.
    let scenario = SCENARIOS
        .iter()
        .find(|scenario| scenario.name == "takeover_fences_the_old_holder")
        .expect("the takeover scenario is not in the table");
    assert_eq!(
        scenario.needs,
        &[
            "create_item",
            "claim",
            "note",
            "heartbeat",
            "finish",
            "show"
        ],
        "the takeover check no longer reaches the stale holder's fenced writes"
    );
}

/// The third control: the reason names the operation it refused.
///
/// With [`store_contract::StoreError::MissingImplementation`]'s text switched to
/// `store has no implementation` — the reason without the operation — this fails,
/// and every group test and
/// [`the_missing_store_refuses_every_operation_by_name`] fails with it. A suite
/// that says only "no store" cannot tell whoever reads it which of eighteen
/// operations a backend still owes, which is the point of running the checks
/// before the store exists.
#[test]
fn control_c_the_reason_names_the_operation_it_refused() {
    use store_contract::StoreError;

    for operation in OPERATIONS {
        let refused = StoreError::MissingImplementation { operation };
        assert_eq!(
            refused.to_string(),
            missing(operation),
            "the refusal has to carry the operation, or a reader cannot tell which one"
        );
    }
}

/// The fourth control, for the half the other three cannot reach: an answer that
/// should not have come.
///
/// Controls A, B and C all decide what the runner does with an *error*. This one
/// decides what it does with an `Ok` where the check asked for a refusal, and it
/// exists because that half had no test: switching [`harness::Run::refused`]'s
/// `Ok` arm to report success left all fourteen other tests green, because with
/// no store every check stops at its very first call and never reaches a refusal
/// to check. A repo rule says a control half whose removal fails no test gets
/// its own test, so the suite drives one here — through [`harness::Partial`],
/// which answers the four operations this check needs, so the close the commit
/// policy must hold is allowed. The check has to report the allowance as its own
/// failure; a runner that called it success would pass a store that closes
/// anything, which is exactly the failure mode a close policy exists to prevent.
#[test]
fn control_d_a_refusal_that_was_allowed_is_reported() {
    let clock = Clock::new();
    let store = Partial::new(
        &["set_close_policy", "create_item", "claim", "finish"],
        clock.clone(),
    );
    let problems = harness::block_on(
        close_policy_checks::under_a_commit_policy_an_item_without_one_stays_held(
            &store,
            &clock,
            "control-d",
        ),
    );
    assert_eq!(
        problems,
        vec!["expected a close with no commit, got ()".to_string()],
        "the check did not report the close it was asked to refuse for a commit"
    );
    // The answer came from a store that was built to answer, not from a missing
    // implementation: this is the case where a backend decided to allow.
    for problem in &problems {
        assert!(
            !problem.contains("store has no implementation"),
            "a missing implementation was reported where the stub answered: {problem}"
        );
    }
}

/// The two checks whose whole point is a race, and which the two controls below
/// drive against a store that fails them. Named as one list so that a third race
/// check has to be added here, or the file says it drives races it does not.
const RACE_CHECKS: [&str; 2] = [
    "one_holder_wins_the_race",
    "claim_next_gives_nothing_out_twice",
];

/// Every race check is still in the suite, in the group that runs it, and stops
/// at the operation its control drives past.
///
/// This is the half that keeps the two controls honest: a control drives a check
/// against a store that answers, and if the check were renamed, moved or deleted
/// the control would be driving something else. The rows' `first` is what the
/// same check reports against the missing store, which is what the group tests
/// assert, so this file states both drives of each race check in one place.
#[test]
fn every_race_check_is_in_the_suite_it_is_driven_against() {
    for name in RACE_CHECKS {
        let scenario = SCENARIOS
            .iter()
            .find(|scenario| scenario.name == name)
            .unwrap_or_else(|| panic!("{name} is driven by a control but is not in the suite"));
        assert_eq!(
            scenario.group, "lease_checks",
            "{name} is driven as a lease check but sits in group {:?}",
            scenario.group
        );
        assert_eq!(
            scenario.first, "create_item",
            "{name} is driven past create_item by its control, so the operation the missing \
             store stops it at has to be that one — otherwise the control's store is answering \
             an operation the suite says it never reaches"
        );
        assert!(
            scenario.needs.contains(&"claim") || scenario.needs.contains(&"claim_next"),
            "{name} is a race check that no longer claims anything"
        );
    }
}

/// The race driver, measured on its own and without a store.
///
/// Errai's two race tests spawn forty calls. The port's driver releases forty
/// threads from one start gate (`harness::StartGate`) and will not let a racer
/// issue its call until all forty have reached it, because a `for … await` loop
/// cannot fail a lease race however the store is built: each call is answered
/// before the next is issued, the store is never asked to choose between two
/// callers, and the checks pass against a backend that hands an item to everyone.
///
/// Nothing else in the suite can show the difference today. Both race checks stop
/// at `create_item` against the missing store, so everything downstream of the
/// driver — including the assertions in [`control_e_a_store_that_holds_one_item_for_every_claimant_is_reported`]
/// — runs only against a stub, and a driver that had quietly stayed a sequential
/// loop would fail no test at all. So this drives [`harness::race`] itself with
/// futures that count instead of calling anything, and asserts how many racers it
/// held inside their call at one instant.
///
/// Two mutations were run against it. Forcing every racer's arrival check to fail,
/// which is what a drive that did not overlap reports, fails this test and both
/// race controls. Folding that refusal back into `answers` instead of `problems` —
/// the other half of the same rule — does not get away either, because the counts
/// below then read zero answered racers; what the `problems` half buys is the
/// report. With it a failed drive says how many racers reached their call and how
/// many were expected, which is the sentence that tells a reader the driver is
/// broken rather than the store wrong; without it the failure says only that
/// nothing answered.
#[test]
fn the_race_driver_releases_every_racer_at_once() {
    let overlap = harness::Reached::new(harness::RACERS);
    let drives = harness::driver_reached(harness::RACERS, &overlap);
    assert!(
        drives.problems.is_empty(),
        "the drive reported problems, so the count below describes a smaller race than it \
         names: {:?}",
        drives.problems
    );
    let peaks: Vec<usize> = drives.answers.iter().flatten().copied().collect();
    assert_eq!(
        peaks.len(),
        harness::RACERS,
        "the drive asked for {} racers and answered {}",
        harness::RACERS,
        peaks.len()
    );
    let peak = peaks
        .iter()
        .copied()
        .max()
        .expect("a drive with no racers was rejected by the assertion above");
    assert_eq!(
        peak,
        harness::RACERS,
        "{} racers were driven and the most ever inside their call at one instant was {peak}: \
         one after another is not a race, and every assertion over the race answers would pass \
         against any backend",
        harness::RACERS,
    );
    // And every racer must be able to say so, not just the one that happened to be
    // scheduled for the instant. A drive where one racer reports less than the rest
    // is a drive that overlapped while that thread was not looking, which is a
    // measurement it lost rather than a race that failed — and it is what used to
    // cost the whole wait bound, so it is asserted here instead of only in the
    // elapsed time of a run.
    let blind: Vec<usize> = peaks
        .iter()
        .enumerate()
        .filter(|(_, seen)| **seen != peak)
        .map(|(racer, _)| racer)
        .collect();
    assert!(
        blind.is_empty(),
        "racers {blind:?} of {} reported a different overlap from the {peak} the drive \
         measured, so they missed it and are reporting what they happened to read; the answers \
         were {peaks:?}",
        harness::RACERS,
    );
    // The same fact, published: the racers did not each have to read forty for the
    // drive to count as a race, which is what a racer that was descheduled through
    // the whole of it now exits on — see
    // `a_counted_racer_that_missed_the_overlap_exits_on_what_others_measured`.
    assert!(
        overlap.full_seen() && overlap.full_at().is_some(),
        "the peak reached {} but the overlap was never stamped as measured, so a late racer \
         would have had nothing to exit on",
        harness::RACERS,
    );
    assert_eq!(
        overlap.inside_now(),
        0,
        "a racer left the counter raised, so its exit was skipped and the count above counted \
         racers that had already finished"
    );
}

/// A counted racer that was not scheduled while the others came and went still
/// exits promptly, on what they measured.
///
/// This is the shape a CPU-quota container produces on its own, and the reason the
/// counters no longer trust a single thread's reading: a racer descheduled between
/// raising the shared counter and reading it back never sees the overlap itself,
/// and then waits. It waited — an independent review measured a healthy round-3 run
/// of these tests at `10.04s`, and this lane measured `10.01s` the same way, both
/// green, both spending nearly all of it inside a wait of this shape. Round 4
/// widened the bound from 10 s to 60 s so that a healthy drive could not be
/// *reported* as not-a-race, which left the same stall free to cost a minute. The
/// fix for the stall is not a bigger bound: it is that every arrival and every
/// reading raises one shared peak, and a racer starts from what has been measured
/// rather than from what this thread happens to see.
///
/// Real starvation cannot be scheduled, so [`harness::driver_reached_lagged`]
/// parks one racer for three seconds before it issues its call, and the counters
/// measure an overlap of two out of three — small on purpose, because the point is
/// that the two other racers measure the overlap and *finish* while the parked one
/// is still asleep, which a forty-racer drive does not guarantee. Everything in the
/// run is then forced:
///
/// * the overlap is measured at about t=0, while the parked racer is asleep, which
///   the stamped `full_at` places inside the park window;
/// * the parked racer wakes at t≈3s, arrives as the only counted racer, and reads
///   `inside == 1`; the two racers that measured the overlap have finished, so the
///   counter only falls from there and no reading of it can reach 2 again;
/// * so the drive finishing well inside its 60 s bound means that racer left on the
///   shared peak. Two switch-offs are run against this, each measured: starting the
///   racer from its own arrival instead of from the shared peak makes this test fail
///   at `63.00s` (three seconds of park plus the sixty-second bound), and taking the
///   loop's value from this thread's own reading instead of the shared peak makes
///   `the_race_driver_releases_every_racer_at_once` run past its bound — `has been
///   running for over 60 seconds`, green but 60.01s, which is the shape the earlier
///   rounds measured at 10.01s and 10.04s when the bound was 10 s.
#[test]
fn a_counted_racer_that_missed_the_overlap_exits_on_what_others_measured() {
    /// How many racers this drive starts: one more than the overlap, so the parked
    /// racer is not needed to complete it.
    const RACERS: usize = 3;
    /// How many inside at once the counters are looking for.
    const TARGET: usize = 2;
    /// How long the racer is parked, standing in for a deschedule.
    const LATE_FOR: Duration = Duration::from_secs(3);
    /// What a prompt drive has to finish inside: well past the three-second park,
    /// an order of magnitude inside the 60 s the wait is bounded by.
    const PROMPT: Duration = Duration::from_secs(20);
    let overlap = harness::Reached::new(TARGET);
    let started = Instant::now();
    let drives = harness::driver_reached_lagged(RACERS, &overlap, 1, LATE_FOR);
    let took = started.elapsed();
    let problems = drives.problems.clone();
    assert!(
        problems.is_empty(),
        "the lagged drive reported problems, so what follows describes a smaller drive than \
         it names: {problems:?}"
    );
    let answered = drives.answers.iter().flatten().count();
    assert_eq!(
        answered, RACERS,
        "the lagged drive answered {answered} of {RACERS} racers, so one never returned"
    );
    let peak = drives
        .answers
        .iter()
        .flatten()
        .copied()
        .max()
        .expect("the assertion above proved there is an answer");
    assert!(
        peak >= TARGET,
        "the counters never measured the overlap of {TARGET} this drive was built to force \
         (the most ever inside was {peak}), so the timing below would prove nothing",
    );
    let full_at = overlap
        .full_at()
        .expect("the peak reached the target above, so the measurement was stamped");
    assert!(
        full_at < started + LATE_FOR,
        "the overlap was first measured {}ms into the drive, after the lagging racer had \
         already resumed at {}ms, so this run never made a racer miss it",
        full_at.duration_since(started).as_millis(),
        LATE_FOR.as_millis(),
    );
    assert!(
        took < PROMPT,
        "the lagged drive took {took:?} although the overlap was measured while the parked \
         racer was still asleep, so a racer that missed the count is still waiting on its own \
         reading instead of on what the others measured",
    );
    assert_eq!(
        overlap.inside_now(),
        0,
        "a counted racer left without lowering the counter, so the overlap measured here is \
         not the one the drive had"
    );
}

/// A racer whose thread cannot be started is reported, not waited for forever.
///
/// The driver used to hold its racers on a fixed-count barrier, and a barrier
/// releases nobody until every racer arrives. That made the failure the driver
/// already claimed to report — `racer N could not start` — unreachable: the
/// racers that *had* started waited for a fortieth that was never going to come,
/// the driver blocked joining them, and the run hung instead of saying what had
/// gone wrong. The gate now opens on a call-off as well as on every arrival, and a
/// refused spawn calls the drive off before it reports itself.
///
/// [`harness::race_with_spawn_refused_from`] makes the refusal happen for real.
/// The racers below the cutoff are ordinary threads with the platform's default
/// stack; the one at the cutoff asks for [`harness::STACK_NO_THREAD`] bytes, which
/// the operating system refuses. Both halves are deterministic — no thread or
/// memory limit is being raced, and the size is one no thread can be given — and
/// the drive fails at racer 39 of 40, the shape the two race checks have.
///
/// Four things are asserted, in the order the failure could hide:
///
/// * the drive returns. This test sets no timeout of its own: under the barrier
///   there was nothing to time out, the process simply stopped, and that is the
///   failure being prevented here;
/// * the refusal is in `problems`, naming the racer and carrying the operating
///   system's own reason;
/// * every racer that did start reports the call-off, quoting that same reason,
///   rather than hanging or reporting a race it never had;
/// * nothing was issued — no racer reached its call. A drive that let half its
///   racers through would leave leases held by a drive that no later read can
///   tell was abandoned.
#[test]
fn a_racer_that_could_not_start_is_reported_instead_of_hanging_the_drive() {
    // A lost call-off would leave the 39 started racers waiting for the gate's
    // minute-long deadline. Give ordinary thread startup room while requiring
    // this forced refusal to finish well before that fallback can hide it.
    const PROMPT: Duration = Duration::from_secs(20);
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let issued = AtomicUsize::new(0);
    let started = Instant::now();
    let drives = harness::race_with_spawn_refused_from(
        &store,
        harness::RACERS,
        harness::RACERS - 1,
        |_, _| {
            let issued = &issued;
            Box::pin(async move { issued.fetch_add(1, Ordering::SeqCst) + 1 })
        },
    );
    let took = started.elapsed();
    assert!(
        took < PROMPT,
        "the forced spawn refusal took {took:?}, close to the start gate's minute-long \
         fallback; started racers were not released promptly"
    );
    let refused: Vec<&String> = drives
        .problems
        .iter()
        .filter(|problem| {
            problem.starts_with(&format!("racer {} could not start: ", harness::RACERS - 1))
        })
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "exactly one racer was asked for a stack no thread can have, so exactly one refusal \
         was expected; the drive reported {:?} with {} answers",
        drives.problems,
        drives.answers.iter().flatten().count()
    );
    let refusal = refused[0].clone();
    assert!(
        refusal.contains("(os error "),
        "the refusal dropped the reason the operating system gave, so a reader cannot tell a \
         refused stack from any other failure to start a thread: {refusal}"
    );
    let called_off: Vec<&String> = drives
        .problems
        .iter()
        .filter(|problem| problem.contains("the drive was called off before it started"))
        .collect();
    assert_eq!(
        called_off.len(),
        harness::RACERS - 1,
        "every racer that started had to report the call-off and leave; the drive reported \
         {:?} with {} answers",
        drives.problems,
        drives.answers.iter().flatten().count()
    );
    for problem in &called_off {
        assert!(
            problem.contains("could not start"),
            "a racer reported that the drive was called off without the reason that caused \
             it: {problem}"
        );
    }
    for problem in &drives.problems {
        assert!(
            !problem.contains("reached their call at the same instant"),
            "a racer got past the gate of a drive that could not have raced, so the gate did \
             not hold it: {problem}"
        );
    }
    let answered = drives.answers.iter().flatten().count();
    assert_eq!(
        drives.passed, 0,
        "{} racers were released by the gate of a drive whose last thread could not start, so \
         the call-off did not hold them",
        drives.passed,
    );
    assert_eq!(
        answered, 0,
        "{answered} racers of a drive that never started answered anyway, so their calls were \
         issued after the drive was called off"
    );
    let issued = issued.load(Ordering::SeqCst);
    assert_eq!(
        issued, 0,
        "a drive that was called off still issued {issued} calls, so an aborted race left \
         effects behind for the next one to read as its own"
    );
}

/// A racer whose wait at the start gate runs out calls the whole drive off, so a
/// drive that has reported a refusal never goes on to issue calls.
///
/// The gate already gave up waiting: a racer whose bound ran out reported "no call
/// was issued" and left. What it did not do was tell anybody else. The racers still
/// asleep on the condvar were left with a gate that was neither open nor called off,
/// so a racer that arrived afterwards completed the count, opened the gate and
/// released them — into a drive that had just been reported as refused. Those calls
/// are the bad case, not a slower one: effects with no drive behind them, held by a
/// race whose results nobody will ever read, reported by a `Drives` whose problem
/// list says nothing was issued. An aborted drive has to be inert, which is the same
/// rule `a_racer_that_could_not_start_is_reported_instead_of_hanging_the_drive`
/// enforces for the call-off a spawner causes.
///
/// The order of events cannot be waited for, so it is scheduled
/// ([`harness::race_with_arrivals_at`]): a gate that gives up after half a second,
/// and three arrivals staggered so the three racers have three different deadlines.
///
/// * racer 0 arrives at t≈0 and its wait runs out at t≈500ms — the refusal;
/// * racer 1 arrives at t≈700ms, so it is still waiting when racer 2 gets there;
/// * racer 2 arrives at t≈1s, after the refusal and while racer 1 is still waiting,
///   and it is the arrival that would have opened the gate.
///
/// Every interval is 200 ms or more, so the schedule holds under the CPU quota
/// rather than only on an idle machine, and the gate bound is injected so the whole
/// drive costs about a second instead of the 60 s a real one would. Each racer calls
/// `create_item` on the missing store for real, so "nothing was issued" is measured
/// at the drive's answers as well as at a counter incremented on the way in.
///
/// On the shipped tree the drive ends in about a second with three problems — the one
/// racer whose wait ran out reporting the timeout, the other two reporting the
/// call-off that quotes it — nobody released by the gate (`passed`), no answers and no
/// calls.
///
/// Two switch-offs were run against this, both measured. Removing the timeout's
/// call-off from [`harness::StartGate::arrive`] — the shape this code had before, and
/// the defect — makes this test fail naming two racers released by the gate of a drive
/// that had already refused. What that run also shows is that their calls did not
/// reach the store: a *separate* rule in the driver, the one that refuses a drive whose
/// racers were not in fact all inside together, caught them instead — after burning its
/// whole 60 s bound, which is why that run fails at 61.00s. So with only that first
/// control missing, the leak is contained by a second one, slowly. To show what the
/// released racers do when nothing else stands in their way, the second run switched
/// off both, and the same drive reported "2 of them reached the store and 2 answered":
/// two `create_item` calls issued by a drive whose report said no call was issued. That
/// second switch-off is not this test's control — it removes a different rule, which
/// `the_race_driver_releases_every_racer_at_once` and the race checks own — but it is
/// what makes the first one's cost visible: the timeout's call-off is the control that
/// makes a refusal both immediate and inert, and the backstop behind it costs a minute.
#[test]
fn a_gate_that_times_out_calls_the_drive_off_before_its_racers_issue() {
    /// Three deadlines need three racers: one whose wait runs out, one still waiting
    /// when the late racer arrives, and the late arrival itself.
    const RACERS: usize = 3;
    /// How long the gate waits before a racer gives up. This is the margin every
    /// event below is spaced against, and it is what an injected bound is for: the
    /// real drive waits [`START_WAIT`](harness) for a measured reason, and a test
    /// that watched a timeout at that bound would spend a minute on it.
    const GATE_WAIT: Duration = Duration::from_millis(500);
    /// When each racer arrives at the gate, which is what separates the deadlines:
    /// racer 0's runs out at ≈500ms, racer 1 arrives at ≈700ms so its own runs out
    /// at ≈1200ms, and racer 2 arrives at ≈1s — after the refusal, before racer 1's.
    const ARRIVALS: [Duration; RACERS] = [
        Duration::ZERO,
        Duration::from_millis(700),
        Duration::from_millis(1000),
    ];
    /// What a prompt drive has to finish inside: well past the last arrival, and an
    /// order of magnitude inside the bound a racer would otherwise wait out.
    const PROMPT: Duration = Duration::from_secs(10);
    /// What the refusal has to say, in every racer's report of it.
    const REFUSAL: &str = "the drive was called off before it started";
    use store_contract::Draft;

    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    let issued = AtomicUsize::new(0);
    let started = Instant::now();
    let drives =
        harness::race_with_arrivals_at(&store, RACERS, GATE_WAIT, &ARRIVALS, |store, racer| {
            let issued = &issued;
            Box::pin(async move {
                let id = format!("gate-{racer}");
                issued.fetch_add(1, Ordering::SeqCst);
                store
                    .create_item(
                        harness::atom(),
                        format!("create-{id}"),
                        Draft::new(&id, "synthetic"),
                    )
                    .await
            })
        });
    let took = started.elapsed();
    let problems = drives.problems.clone();
    let answered = drives.answers.iter().flatten().count();
    let issued = issued.load(Ordering::SeqCst);
    // Which report is which: the racer whose own wait ran out reports the timeout as
    // its own, exactly as a refused spawn reports itself, and every other racer
    // reports the call-off that quotes that reason. So the counts below say both
    // that the timeout happened once and that it reached everybody.
    let cause = "racers to arrive at the start gate and only";
    let timeouts: Vec<&String> = problems
        .iter()
        .filter(|problem| problem.contains(cause) && !problem.contains(REFUSAL))
        .collect();
    let called_off: Vec<&String> = problems
        .iter()
        .filter(|problem| problem.contains(REFUSAL))
        .collect();

    assert_eq!(
        drives.passed, 0,
        "{} racers were released by the start gate of a drive that had already refused, so \
         the timeout was reported and the drive carried on: {issued} of them reached the store \
         and {answered} answered; the drive reported {problems:?}",
        drives.passed,
    );
    assert_eq!(
        issued, 0,
        "{issued} racers of a drive whose gate timed out still reached the store, so the \
         refusal was reported and the calls were issued anyway: {problems:?}",
    );
    assert_eq!(
        answered, 0,
        "{answered} racers answered a drive that never started, so their calls reached the \
         store and were answered: {problems:?}",
    );
    assert_eq!(
        timeouts.len(),
        1,
        "exactly one racer's wait at the gate was supposed to run out, and its reason was \
         supposed to call off the whole drive; the drive reported {problems:?} with {answered} \
         answers after {took:?}",
    );
    assert_eq!(
        called_off.len(),
        RACERS - 1,
        "every other racer was supposed to report the call-off and leave; the drive reported \
         {problems:?} with {answered} answers after {took:?}",
    );
    assert_eq!(
        problems.len(),
        RACERS,
        "every racer of a drive whose gate timed out was supposed to report and leave; the \
         drive reported {problems:?} with {answered} answers",
    );
    for problem in &called_off {
        assert!(
            problem.contains(cause),
            "a racer reported that the drive was called off without the gate timeout that \
             caused it: {problem}"
        );
    }
    for problem in &problems {
        assert!(
            !problem.contains("reached their call at the same instant"),
            "a racer got past the gate of a drive whose gate never opened, so the gate did \
             not hold it: {problem}"
        );
    }
    assert!(
        took >= ARRIVALS[RACERS - 1],
        "the drive finished after {took:?}, before the last racer was even scheduled to \
         arrive at {:?}, so nothing in this run was late and the assertions above prove \
         nothing",
        ARRIVALS[RACERS - 1],
    );
    assert!(
        took < PROMPT,
        "the drive took {took:?} to refuse, so a racer that was released by the call-off \
         still waited on something instead of leaving with the refusal",
    );
}

/// The fifth control: a backend that lets every claimant hold one item.
///
/// The race checks are the port of Errai's forty-clients race, and against the
/// missing store they cannot fail — both stop at `create_item`, so "exactly one
/// claimant holds it" is an assertion no store has ever met. An assertion that
/// has never been able to fail is exactly where a backend passes conformance by
/// accident, so the check is driven here against [`harness::NonAtomic`]: a store
/// that tells all forty claimants they hold the item and records one holder. The
/// check reports the forty. Reserve the item on first touch inside the stub's
/// `claim` and this control stops failing, which is this control tested by
/// removing it.
#[test]
fn control_e_a_store_that_holds_one_item_for_every_claimant_is_reported() {
    let clock = Clock::new();
    let store = NonAtomic::new(clock.clone());
    let problems = harness::block_on(lease_checks::one_holder_wins_the_race(
        &store,
        &clock,
        "control-e",
    ));
    // The stub really was as defective as the check has to catch — forty leases
    // told out against one holder recorded. Asserted from the store itself, so
    // the check's report cannot be passing for the wrong reason.
    assert_eq!(
        store.leases_issued(),
        harness::RACERS,
        "the stub let {} claimants hold the item instead of all {}, so the check had no \
         non-atomic claim to catch and its silence would mean nothing",
        store.leases_issued(),
        harness::RACERS,
    );
    assert_eq!(
        store.holders_recorded(),
        1,
        "the stub records {} holders; the mismatch this control needs is forty leases against \
         one",
        store.holders_recorded(),
    );
    assert_eq!(
        problems.len(),
        1,
        "the check reported {problems:?} against a store that told every claimant it holds \
         control-e"
    );
    assert!(
        problems[0].starts_with(&format!(
            "expected one holder of control-e, got {}",
            harness::RACERS
        )),
        "the check did not report every claimant being handed the item: {:?}",
        problems[0]
    );
    for problem in &problems {
        assert!(
            !problem.contains("store has no implementation"),
            "the race stopped at an absent operation instead of reaching the claim: {problem}"
        );
    }
}

/// The sixth control, for the other race: an item given out twice.
///
/// Same shape as [`control_e_a_store_that_holds_one_item_for_every_claimant_is_reported`],
/// for the assertion Errai's second race test makes. [`harness::NonAtomic`]'s
/// `claim_next` reserves nothing for the callers that arrived together, so all
/// forty are given the same item; the check counts one distinct item among forty
/// and says so. Let the stub reserve before it answers and this control stops
/// failing.
#[test]
fn control_f_an_item_handed_out_twice_is_reported() {
    let clock = Clock::new();
    let store = NonAtomic::new(clock.clone());
    let problems = harness::block_on(lease_checks::claim_next_gives_nothing_out_twice(
        &store,
        &clock,
        "control-f",
    ));
    assert_eq!(
        store.leases_issued(),
        harness::RACERS,
        "the stub handed out {} items to racers instead of {}, so no item went out twice and \
         this control proved nothing",
        store.leases_issued(),
        harness::RACERS,
    );
    assert_eq!(
        problems,
        vec![
            "40 racers took 15 items and only 1 of them are distinct, so an item went out twice"
                .to_string()
        ],
        "the check did not report the item that went out twice"
    );
}

/// `Unavailable` cannot be mistaken for a store: it answers the contract and
/// nothing else, so it cannot satisfy an assertion by accident.
#[test]
fn the_missing_store_refuses_every_operation_by_name() {
    let clock = Clock::new();
    let store = Unavailable::new(clock.clone());
    for operation in OPERATIONS {
        let reported = harness::block_on(refuse_one(&store, operation));
        assert_eq!(
            reported,
            missing(operation),
            "{operation} did not answer with the missing-implementation reason"
        );
    }
    // The clock it was handed is the clock the checks move, which is how a lease
    // expires in a check without a sleep.
    let before = store.now();
    clock.advance(Duration::from_secs(5));
    assert!(
        store.now() > before,
        "the suite's clock did not move, so expiry would be a guess"
    );
}

/// Ask the missing store for one operation by name and report what it said.
async fn refuse_one(store: &Unavailable, operation: &str) -> String {
    use store_contract::Draft;
    use store_contract::EdgeKind;
    use store_contract::Evidence;
    use store_contract::Finish;
    use store_contract::Lease;
    use store_contract::Status;
    use store_contract::StoreError;

    let lease = Lease {
        item_id: "x".to_string(),
        claimant: "claimant".to_string(),
        session: 1,
        epoch: 1,
        acquisition: "acquisition".to_string(),
    };
    let refused: Result<(), StoreError> = match operation {
        "create_item" => store
            .create_item(
                harness::atom(),
                "request".to_string(),
                Draft::new("x", "title"),
            )
            .await
            .map(|_| ()),
        "note" => {
            store
                .note(
                    harness::atom(),
                    lease,
                    "request".to_string(),
                    "text".to_string(),
                )
                .await
        }
        "add_edge" => {
            store
                .add_edge(
                    harness::atom(),
                    "x".to_string(),
                    EdgeKind::Blocks,
                    "y".to_string(),
                )
                .await
        }
        "set_status" => {
            store
                .set_status(harness::person(), "x".to_string(), Status::Deferred)
                .await
        }
        "show" => store
            .show(harness::atom(), "x".to_string())
            .await
            .map(|_| ()),
        "all_items" => store.all_items(harness::atom()).await.map(|_| ()),
        "ready" => store.ready(harness::atom(), 10).await.map(|_| ()),
        "claim" => store
            .claim(
                harness::atom(),
                "claimant".to_string(),
                "x".to_string(),
                Duration::from_secs(60),
            )
            .await
            .map(|_| ()),
        "claim_next" => store
            .claim_next(
                harness::atom(),
                "claimant".to_string(),
                Duration::from_secs(60),
            )
            .await
            .map(|_| ()),
        "heartbeat" => store.heartbeat(lease, Duration::from_secs(60)).await,
        "finish" => store.finish(lease, Finish::Close).await,
        "sweep" => store.sweep(harness::atom()).await.map(|_| ()),
        "remember" => {
            store
                .remember(
                    harness::atom(),
                    "request".to_string(),
                    "key".to_string(),
                    "value".to_string(),
                )
                .await
        }
        "prompt_memories" => store.prompt_memories(harness::atom(), 10).await.map(|_| ()),
        "set_close_policy" => {
            store
                .set_close_policy(harness::person(), vec![Evidence::Commit])
                .await
        }
        "set_item_close_policy" => {
            store
                .set_item_close_policy(
                    harness::person(),
                    "x".to_string(),
                    vec![Evidence::VerifiedCommit],
                    Some("refs/heads/main".to_string()),
                )
                .await
        }
        "link_commit" => store.link_commit(lease, "c".repeat(40)).await,
        "import" => store
            .import(harness::atom(), String::new(), false)
            .await
            .map(|_| ()),
        "export" => store.export(harness::atom()).await.map(|_| ()),
        other => panic!("no such operation to ask about: {other}"),
    };
    match refused {
        Ok(value) => format!("answered {value:?}"),
        Err(error) => error.to_string(),
    }
}

/// What the runner reports for a store that has no `operation`.
fn missing(operation: &str) -> String {
    format!("store has no implementation: {operation}")
}

/// Every behavioral group, including the two forty-client races, runs against
/// a fresh persistent SQLite database and the deterministic suite clock.
fn sqlite_authorized_for_tests(
    path: &std::path::Path,
    now: impl Fn() -> std::time::SystemTime + Send + Sync + 'static,
) -> Result<atomic_graph::sqlite::SqliteStore, store_contract::StoreError> {
    let database = path.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let durable = atomic_durable::runtime::Durare::sqlite(&database)
                .await
                .expect("durable store");
            let credential = atomic_durable::gate::HumanCredential::new(
                atomic_durable::gate::Principal::local("owner"),
                "graph conformance operator secret",
            );
            atomic_durable::gate::enroll(&durable, &credential)
                .await
                .expect("enrolled graph test operator");
        });
    })
    .join()
    .expect("enrollment worker");
    atomic_graph::sqlite::SqliteStore::open_authorized(
        path,
        now,
        atomic_durable::gate::HumanCredential::new(
            atomic_durable::gate::Principal::local("owner"),
            "graph conformance operator secret",
        ),
    )
}

#[test]
fn sqlite_graph_first_open_uses_the_durable_migration_history() {
    use sqlx::Connection;
    use store_contract::{Actor, Draft, GraphStore};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-first-migration-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("fresh graph open must migrate the shared store");
    harness::block_on(async {
        store
            .create_item(
                Actor::agent("setup"),
                "first-item".into(),
                Draft::new("at-first", "First"),
            )
            .await
            .expect("graph tables are available after migration");
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open migration reader");
        let version: i64 = sqlx::query_scalar("SELECT version FROM atomic_store_version")
            .fetch_one(&mut conn)
            .await
            .expect("durable version marker");
        assert_eq!(version, atomic_durable::store::STORE_VERSION);
        let graph_history: String =
            sqlx::query_scalar("SELECT name FROM atomic_store_history WHERE version=12")
                .fetch_one(&mut conn)
                .await
                .expect("work-graph migration history");
        assert_eq!(graph_history, "work-graph");
        let index_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('atomic_graph_edges_by_target','atomic_graph_ready_order')",
        )
        .fetch_one(&mut conn)
        .await
        .expect("graph migration indexes");
        assert_eq!(index_count, 2);
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_graph_open_refuses_a_newer_marker_before_it_returns_a_store() {
    use sqlx::Connection;
    use store_contract::{Actor, Draft, GraphStore};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-newer-migration-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("open current graph store");
    harness::block_on(store.create_item(
        Actor::agent("setup"),
        "first-item".into(),
        Draft::new("at-first", "First"),
    ))
    .expect("create existing graph item");
    drop(store);
    let newer = atomic_durable::store::STORE_VERSION + 1;
    harness::block_on(async {
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open marker writer");
        sqlx::query("UPDATE atomic_store_version SET version=?")
            .bind(newer)
            .execute(&mut conn)
            .await
            .expect("mark future schema");
    });
    let error = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .err()
        .expect("a future schema must refuse graph open");
    assert!(
        error
            .to_string()
            .contains("newer than this atomic understands"),
        "unexpected migration refusal: {error}"
    );
    harness::block_on(async {
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open unchanged store");
        let marker: i64 = sqlx::query_scalar("SELECT version FROM atomic_store_version")
            .fetch_one(&mut conn)
            .await
            .expect("version still present");
        let item_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items")
            .fetch_one(&mut conn)
            .await
            .expect("existing item still present");
        assert_eq!((marker, item_count), (newer, 1));
    });
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_graph_open_refuses_unversioned_graph_tables_before_migrating() {
    use sqlx::Connection;

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-unversioned-migration-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    std::fs::File::create(&path).expect("create unversioned database");
    harness::block_on(async {
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open unversioned database");
        sqlx::query("CREATE TABLE atomic_graph_items (id TEXT PRIMARY KEY, title TEXT)")
            .execute(&mut conn)
            .await
            .expect("plant incompatible graph table");
    });
    let error = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .err()
        .expect("unversioned graph table must refuse open");
    assert!(
        error.to_string().contains("unrecognized atomic schema"),
        "unexpected migration refusal: {error}"
    );
    harness::block_on(async {
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("reopen rejected database");
        let version_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='atomic_store_version'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("read version table inventory");
        assert_eq!(version_tables, 0, "refusal must not stamp a version");
    });
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_conforms_to_the_behavioral_scenarios() {
    let mut all_failures = Vec::new();
    for (index, scenario) in SCENARIOS.iter().enumerate() {
        let clock = Clock::new();
        let path = std::env::temp_dir().join(format!(
            "atomic-graph-conformance-{}-{index}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let observed = clock.clone();
        let store =
            sqlite_authorized_for_tests(&path, move || observed.now()).expect("open SQLite graph");
        let failures = harness::block_on(dispatch(scenario.name, &store, &clock, scenario.at));
        let _ = std::fs::remove_file(&path);
        if !failures.is_empty() {
            all_failures.push(format!("{}: {failures:#?}", scenario.name));
        }
    }
    assert!(all_failures.is_empty(), "{}", all_failures.join("\n"));
}

#[test]
fn sqlite_roundtrips_draft_fields_and_every_edge_kind() {
    use store_contract::{Actor, Draft, EdgeKind, Status, Trust};
    let clock = Clock::new();
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-roundtrip-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let observed = clock.clone();
    let store = sqlite_authorized_for_tests(&path, move || observed.now()).expect("test operation");
    let mut draft = Draft::new("at-fields", "Every field");
    draft.body = "body and acceptance".to_owned();
    draft.kind = "feature".to_owned();
    draft.priority = -3;
    draft.parent = Some("at-parent".to_owned());
    draft.due_at = Some(clock.now() + Duration::from_secs(120));
    draft.notes = vec!["first".to_owned(), "second".to_owned()];
    let parent = Draft::new("at-parent", "Parent");
    let target = Draft::new("at-target", "Target");
    let actor = Actor::human("maintainer");
    harness::block_on(async {
        store
            .create_item(actor.clone(), "parent".to_owned(), parent)
            .await
            .expect("test operation");
        store
            .create_item(actor.clone(), "target".to_owned(), target)
            .await
            .expect("test operation");
        let item = store
            .create_item(actor.clone(), "fields".to_owned(), draft.clone())
            .await
            .expect("test operation");
        assert_eq!(item.title, draft.title);
        assert_eq!(item.body, draft.body);
        assert_eq!(item.kind, draft.kind);
        assert_eq!(item.priority, draft.priority);
        assert_eq!(item.status, Status::Open);
        assert_eq!(item.parent, draft.parent);
        assert_eq!(item.due_at, draft.due_at);
        assert_eq!(item.created_at, clock.now());
        assert_eq!(item.created_by, "local:owner");
        assert_eq!(item.trust, Trust::Human);
        for kind in [
            EdgeKind::Blocks,
            EdgeKind::ParentChild,
            EdgeKind::ConditionalBlocks,
            EdgeKind::WaitFor,
            EdgeKind::Related,
            EdgeKind::DiscoveredFrom,
            EdgeKind::RelatesTo,
            EdgeKind::Duplicates,
            EdgeKind::Supersedes,
            EdgeKind::RepliesTo,
        ] {
            store
                .add_edge(
                    actor.clone(),
                    item.id.clone(),
                    kind,
                    if kind == EdgeKind::ParentChild {
                        "at-parent"
                    } else {
                        "at-target"
                    }
                    .to_owned(),
                )
                .await
                .expect("test operation");
        }
        let shown = store
            .show(actor.clone(), item.id.clone())
            .await
            .expect("test operation")
            .expect("test operation");
        assert_eq!(shown.notes.len(), 2);
        assert_eq!(shown.notes[0].text, "first");
        assert_eq!(shown.notes[1].text, "second");
        assert_eq!(shown.blocked_by, ["at-parent", "at-target"]);
        let exported = store.export(actor).await.expect("test operation");
        let line = exported
            .lines()
            .find(|line| line.contains("\"id\":\"at-fields\""))
            .expect("test operation");
        let json: serde_json::Value = serde_json::from_str(line).expect("test operation");
        let kinds: std::collections::BTreeSet<_> = json["dependencies"]
            .as_array()
            .expect("test operation")
            .iter()
            .map(|edge| edge["type"].as_str().expect("test operation"))
            .collect();
        assert_eq!(kinds.len(), 10);
    });
    drop(store);
    let reopened = sqlite_authorized_for_tests(&path, move || clock.now()).expect("test operation");
    let shown =
        harness::block_on(reopened.show(Actor::human("maintainer"), "at-fields".to_owned()))
            .expect("test operation")
            .expect("test operation");
    assert_eq!(shown.item.body, draft.body);
    assert_eq!(shown.notes.len(), 2);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_edge_endpoints_and_parent_identity_are_atomic() {
    use store_contract::{Actor, Draft, EdgeKind, StoreError};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-edge-identity-{}.sqlite",
        std::process::id()
    ));
    let imported_path = std::env::temp_dir().join(format!(
        "atomic-graph-edge-identity-import-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&imported_path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph store");
    let actor = Actor::human("maintainer");

    harness::block_on(async {
        for id in ["at-child", "at-parent-one", "at-parent-two"] {
            store
                .create_item(actor.clone(), format!("create-{id}"), Draft::new(id, id))
                .await
                .expect("create graph item");
        }
        let before = store.export(actor.clone()).await.expect("export graph");
        for kind in [EdgeKind::ParentChild, EdgeKind::Blocks] {
            for (from, to, missing) in [
                ("at-absent", "at-parent-one", "at-absent"),
                ("at-child", "at-absent", "at-absent"),
            ] {
                assert!(matches!(
                    store
                        .add_edge(actor.clone(), from.to_owned(), kind, to.to_owned())
                        .await,
                    Err(StoreError::MissingItem { item_id }) if item_id == missing
                ));
                assert_eq!(
                    store
                        .export(actor.clone())
                        .await
                        .expect("export after refusal"),
                    before,
                    "refusing a missing {missing} edge must not write a parent or edge"
                );
            }
        }

        store
            .add_edge(
                actor.clone(),
                "at-child".into(),
                EdgeKind::ParentChild,
                "at-parent-one".into(),
            )
            .await
            .expect("link first parent");
        let shown = store
            .show(actor.clone(), "at-child".into())
            .await
            .expect("show child")
            .expect("child exists");
        assert_eq!(shown.item.parent.as_deref(), Some("at-parent-one"));
        assert_eq!(shown.blocked_by, ["at-parent-one"]);
        let first_parent = store
            .export(actor.clone())
            .await
            .expect("export first parent");
        assert!(matches!(
            store
                .add_edge(
                    actor.clone(),
                    "at-child".into(),
                    EdgeKind::ParentChild,
                    "at-parent-two".into(),
                )
                .await,
            Err(StoreError::InvalidEdge { .. })
        ));
        assert_eq!(
            store
                .export(actor.clone())
                .await
                .expect("export after refusal"),
            first_parent,
            "refusing a second parent must not write an edge or change the item"
        );

        let imported = sqlite_authorized_for_tests(&imported_path, std::time::SystemTime::now)
            .expect("open second graph store");
        imported
            .import(actor.clone(), first_parent, false)
            .await
            .expect("import exported graph");
        let imported_child = imported
            .show(actor, "at-child".into())
            .await
            .expect("show imported child")
            .expect("imported child exists");
        assert_eq!(imported_child.item.parent.as_deref(), Some("at-parent-one"));
        assert_eq!(imported_child.blocked_by, ["at-parent-one"]);
    });
    drop(store);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(imported_path);
}

#[test]
fn sqlite_works_inside_an_existing_tokio_runtime() {
    use store_contract::{Actor, Draft};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test operation");
    runtime.block_on(async {
        let path = std::env::temp_dir().join(format!("atomic-graph-tokio-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
        let actor = Actor::human("owner");
        let item = store.create_item(actor.clone(), "create".to_owned(), Draft::new("at-runtime", "Runtime"))
            .await.expect("test operation");
        assert_eq!(store.show(actor.clone(), item.id).await.expect("test operation").expect("test operation").item.title, "Runtime");
        let input = r#"{"id":"at-import","title":"Imported","description":"Body","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z"}"#;
        let report = store.import(actor.clone(), input.to_owned(), false).await.expect("test operation");
        assert_eq!(report.added, ["at-import"]);
        assert_eq!(store.show(actor, "at-import".to_owned()).await.expect("test operation").expect("test operation").item.body, "Body");
        drop(store);
        let _ = std::fs::remove_file(path);
    });
}

#[test]
fn sqlite_import_export_roundtrips_beads_fields_and_rejects_malformed_file() {
    use store_contract::{Actor, StoreError};
    let path =
        std::env::temp_dir().join(format!("atomic-graph-jsonl-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let actor = Actor::agent("importer");
    let input = concat!(
        r#"{"_type":"issue","id":"at-one","title":"First","description":"Full body","status":"open","priority":1,"issue_type":"epic","created_at":"2026-01-01T00:00:00Z","labels":["alpha"],"custom":{"nested":true},"notes":"A note"}"#,
        "\n",
        r#"{"id":"at-two","title":"Second","description":"Child","status":"closed","priority":3,"issue_type":"task","created_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-03T00:00:00Z","parent_issue":"at-one","dependencies":[{"issue_id":"at-two","depends_on_id":"at-one","type":"related"}]}"#,
        "\n",
        r#"{"_type":"memory","key":"project-note","value":"Remember this"}"#,
        "\n"
    );
    harness::block_on(async {
        let first = store
            .import(actor.clone(), input.to_owned(), false)
            .await
            .expect("test operation");
        assert_eq!(first.added, ["at-one", "at-two"]);
        let second = store
            .import(actor.clone(), input.to_owned(), false)
            .await
            .expect("test operation");
        assert_eq!(second.unchanged, ["at-one", "at-two"]);
        assert!(second.added.is_empty());
        assert_eq!(second.kept.get("memories"), None);
        let output = store.export(actor.clone()).await.expect("test operation");
        let before: Vec<serde_json::Value> = input
            .lines()
            .map(|l| serde_json::from_str(l).expect("test operation"))
            .collect();
        let after: Vec<serde_json::Value> = output
            .lines()
            .map(|l| serde_json::from_str(l).expect("test operation"))
            .collect();
        assert_eq!(after, before);
        let unknown = format!(
            "{input}{}\n{}\n",
            r#"{"_type":"issue","id":"at-never","title":"Never","status":"open"}"#,
            r#"{"_type":"future","id":"at-unknown","title":"Unknown","status":"open"}"#
        );
        assert!(matches!(
            store.import(actor.clone(), unknown, false).await,
            Err(StoreError::ImportRefused(problems))
                if problems.iter().any(|problem| problem.contains("unsupported _type"))
        ));
        assert!(
            store
                .show(actor.clone(), "at-never".to_owned())
                .await
                .expect("test operation")
                .is_none(),
            "an unknown record type must refuse the entire import"
        );
        let bad = format!("{input}{{broken\n");
        assert!(matches!(
            store.import(actor.clone(), bad, false).await,
            Err(StoreError::ImportRefused(_))
        ));
        assert_eq!(
            store
                .all_items(actor.clone())
                .await
                .expect("test operation")
                .len(),
            2
        );
        let dry =
            r#"{"id":"at-dry","title":"Dry","status":"open","priority":2,"issue_type":"task"}"#;
        assert!(matches!(
            store.import(actor.clone(), dry.to_owned(), true).await,
            Err(StoreError::ImportDryRun(_))
        ));
        assert!(
            store
                .show(actor, "at-dry".to_owned())
                .await
                .expect("test operation")
                .is_none()
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_agent_status_cannot_close_defer_or_bypass_close_policy() {
    use sqlx::Connection;
    use store_contract::{Actor, Draft, Evidence, GraphStore, Status, StoreError};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-agent-status-authority-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let human = Actor::human("owner");
    let agent = Actor::agent("atom");
    harness::block_on(async {
        store
            .create_item(
                human.clone(),
                "create-open".into(),
                Draft::new("open", "Open"),
            )
            .await
            .expect("create unprotected item");
        store
            .claim(
                agent.clone(),
                "claim-open".into(),
                "open".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim unprotected item")
            .expect("lease");
        store
            .set_close_policy(human.clone(), vec![Evidence::Commit])
            .await
            .expect("require commit for later item");
        store
            .create_item(
                human.clone(),
                "create-policy".into(),
                Draft::new("policy", "Protected"),
            )
            .await
            .expect("create protected item");

        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open independent reader");
        let before_items: Vec<(String, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT id,status,claim_epoch,closed_at FROM atomic_graph_items WHERE id IN ('open','policy') ORDER BY id",
        )
        .fetch_all(&mut conn)
        .await
        .expect("read item rows");
        let before_leases: Vec<(String, String, i64, i64, String, i64)> = sqlx::query_as(
            "SELECT item_id,claimant,session,epoch,acquisition,expires_at FROM atomic_graph_leases WHERE item_id IN ('open','policy') ORDER BY item_id",
        )
        .fetch_all(&mut conn)
        .await
        .expect("read lease rows");
        assert_eq!(before_items.len(), 2);
        assert_eq!(before_leases.len(), 1);

        for (id, status) in [
            ("open", Status::Closed),
            ("open", Status::Deferred),
            ("policy", Status::Closed),
            ("policy", Status::Deferred),
        ] {
            assert!(matches!(
                store.set_status(agent.clone(), id.into(), status).await,
                Err(StoreError::NeedsHumanCredential { action }) if action == "set status"
            ));
            let after_items: Vec<(String, String, i64, Option<i64>)> = sqlx::query_as(
                "SELECT id,status,claim_epoch,closed_at FROM atomic_graph_items WHERE id IN ('open','policy') ORDER BY id",
            )
            .fetch_all(&mut conn)
            .await
            .expect("read unchanged item rows");
            let after_leases: Vec<(String, String, i64, i64, String, i64)> = sqlx::query_as(
                "SELECT item_id,claimant,session,epoch,acquisition,expires_at FROM atomic_graph_leases WHERE item_id IN ('open','policy') ORDER BY item_id",
            )
            .fetch_all(&mut conn)
            .await
            .expect("read unchanged lease rows");
            assert_eq!(after_items, before_items, "agent changed item rows");
            assert_eq!(after_leases, before_leases, "agent changed lease rows");
        }
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_agent_memory_cannot_replace_human_or_approved_rows() {
    use sqlx::Connection;
    use store_contract::{Actor, GraphStore, StoreError};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-agent-memory-authority-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let human = Actor::human("owner");
    let agent = Actor::agent("atom");
    harness::block_on(async {
        for (request, key, value) in [
            ("human-seed", "human-key", "owner text"),
            ("approved-seed", "approved-key", "approved text"),
        ] {
            store
                .remember(human.clone(), request.into(), key.into(), value.into())
                .await
                .expect("seed trusted memory");
        }
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open independent reader");
        sqlx::query("UPDATE atomic_graph_memories SET trust='approved' WHERE key='approved-key'")
            .execute(&mut conn)
            .await
            .expect("model an existing approved memory");
        let before_memories: Vec<(String, String, String)> =
            sqlx::query_as("SELECT key,value,trust FROM atomic_graph_memories ORDER BY key")
                .fetch_all(&mut conn)
                .await
                .expect("read trusted memories");
        let before_requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_requests")
            .fetch_one(&mut conn)
            .await
            .expect("count requests");
        assert_eq!(before_memories[0].2, "approved");
        assert_eq!(before_memories[1].2, "human");

        for (request, key) in [
            ("agent-replace-human", "human-key"),
            ("agent-replace-approved", "approved-key"),
        ] {
            assert!(matches!(
                store
                    .remember(agent.clone(), request.into(), key.into(), "agent text".into())
                    .await,
                Err(StoreError::NeedsHumanCredential { action }) if action == "replace trusted memory"
            ));
            let after_memories: Vec<(String, String, String)> =
                sqlx::query_as("SELECT key,value,trust FROM atomic_graph_memories ORDER BY key")
                    .fetch_all(&mut conn)
                    .await
                    .expect("read unchanged memories");
            let after_requests: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_requests")
                    .fetch_one(&mut conn)
                    .await
                    .expect("count unchanged requests");
            assert_eq!(
                after_memories, before_memories,
                "agent changed trusted memory"
            );
            assert_eq!(
                after_requests, before_requests,
                "agent recorded rejected request"
            );
        }
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_refuses_untrusted_admin_changes_and_memory_demotion() {
    use store_contract::{Actor, Draft, Finish, GraphStore, Status, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-authority-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let human = Actor::human("owner");
    let agent = Actor::agent("atom");
    harness::block_on(async {
        store
            .remember(
                human.clone(),
                "h1".into(),
                "policy".into(),
                "human words".into(),
            )
            .await
            .expect("test operation");
        assert!(matches!(
            store
                .remember(
                    agent.clone(),
                    "a1".into(),
                    "policy".into(),
                    "agent words".into()
                )
                .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ));
        let memories = store
            .prompt_memories(agent.clone(), 10)
            .await
            .expect("test operation");
        assert_eq!(memories[0].value, "human words");
        store
            .create_item(
                human.clone(),
                "create".into(),
                Draft::new("at-owned", "Owned"),
            )
            .await
            .expect("test operation");
        let lease = store
            .claim(
                agent.clone(),
                "holder".into(),
                "at-owned".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("test operation")
            .expect("test operation");
        assert!(matches!(
            store
                .set_status(agent.clone(), "at-owned".into(), Status::Closed)
                .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ));
        assert_eq!(
            store
                .show(agent.clone(), "at-owned".into())
                .await
                .expect("test operation")
                .expect("test operation")
                .holder
                .expect("test operation")
                .claimant,
            "holder"
        );
        store
            .finish(lease.clone(), Finish::Close)
            .await
            .expect("test operation");
        let mut forged = lease.clone();
        forged.claimant = "intruder".into();
        assert!(matches!(
            store.finish(forged, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        let mut forged = lease.clone();
        forged.acquisition = "forged".into();
        assert!(matches!(
            store.finish(forged, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        store
            .finish(lease, Finish::Close)
            .await
            .expect("test operation");
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_finished_close_replay_fences_full_lease_identity() {
    use sqlx::Connection;
    use store_contract::{Actor, Draft, Finish, GraphStore, StoreError};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-finished-identity-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph store");
    let actor = Actor::agent("worker");
    harness::block_on(async {
        store
            .create_item(
                actor.clone(),
                "create".into(),
                Draft::new("at-done", "Done"),
            )
            .await
            .expect("create item");
        let lease = store
            .claim(
                actor.clone(),
                "holder".into(),
                "at-done".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim item")
            .expect("item was ready");
        store
            .finish(lease.clone(), Finish::Close)
            .await
            .expect("close item");
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("open independent state reader");
        let row = async |conn: &mut sqlx::SqliteConnection| {
            sqlx::query_as::<_, (
                String,
                Option<i64>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<i64>,
                i64,
            )>(
                "SELECT status,last_finish_session,last_finish_epoch,last_finish_claimant,last_finish_acquisition,terminal_outcome,closed_at,(SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-done') FROM atomic_graph_items WHERE id='at-done'",
            )
            .fetch_one(conn)
            .await
            .expect("read finished item and lease state")
        };
        let before = row(&mut conn).await;
        let exported = store.export(actor.clone()).await.expect("export graph");
        store
            .finish(lease.clone(), Finish::Close)
            .await
            .expect("exact close replay is a no-op");
        assert_eq!(row(&mut conn).await, before, "exact replay changed state");
        assert_eq!(
            store.export(actor.clone()).await.expect("export graph"),
            exported
        );

        let mut wrong_claimant = lease.clone();
        wrong_claimant.claimant = "intruder".into();
        assert!(matches!(
            store.finish(wrong_claimant, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        assert_eq!(
            row(&mut conn).await,
            before,
            "forged claimant changed state"
        );
        assert_eq!(
            store.export(actor.clone()).await.expect("export graph"),
            exported
        );

        let mut wrong_acquisition = lease;
        wrong_acquisition.acquisition = "forged".into();
        assert!(matches!(
            store.finish(wrong_acquisition, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        assert_eq!(
            row(&mut conn).await,
            before,
            "forged acquisition changed state"
        );
        assert_eq!(store.export(actor).await.expect("export graph"), exported);
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_import_rejects_wrong_shapes_and_preserves_signed_dates() {
    use store_contract::{Actor, GraphStore, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-import-shape-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let actor = Actor::agent("importer");
    let valid =
        r#"{"id":"at-valid","title":"Valid","status":"open","priority":2,"issue_type":"task"}"#;
    let bad = [
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","due_at":"not-a-date"}"#,
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","due_at":7}"#,
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","dependencies":{}}"#,
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","comments":{}}"#,
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","comments":[{"author":7,"text":"x"}]}"#,
        r#"{"id":"at-bad","title":"Bad","status":"open","priority":2,"issue_type":"task","pinned":"true"}"#,
    ];
    harness::block_on(async {
        for malformed in bad {
            let input = format!("{valid}\n{malformed}\n");
            let refusal = store
                .import(actor.clone(), input, false)
                .await
                .expect_err("malformed second line must refuse the batch");
            assert!(
                matches!(refusal, StoreError::ImportRefused(ref problems)
                    if problems.iter().any(|problem| problem.contains("line 2"))),
                "refusal did not identify the malformed source line: {refusal:?}"
            );
            assert!(
                store
                    .all_items(actor.clone())
                    .await
                    .expect("test operation")
                    .is_empty()
            );
        }
        let before_epoch = r#"{"id":"at-old","title":"Old","status":"open","priority":2,"issue_type":"task","created_at":"1960-01-01T00:00:00Z","due_at":"1960-01-02T00:00:00Z"}"#;
        store
            .import(actor.clone(), before_epoch.into(), false)
            .await
            .expect("test operation");
        let old = store
            .show(actor.clone(), "at-old".into())
            .await
            .expect("test operation")
            .expect("test operation")
            .item;
        assert!(old.created_at < std::time::UNIX_EPOCH);
        assert!(old.due_at.expect("test operation") < std::time::UNIX_EPOCH);
        assert_eq!(
            store.ready(actor, 10).await.expect("test operation")[0].id,
            "at-old"
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_pre_epoch_dates_roundtrip_through_create_import_show_and_export() {
    use store_contract::{Actor, Draft, GraphStore};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-signed-dates-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let created = std::time::UNIX_EPOCH - Duration::from_millis(123_456_789_123);
    let due = created + Duration::from_millis(3_600_123);
    let store = sqlite_authorized_for_tests(&path, move || created).expect("open graph store");
    let actor = Actor::human("maintainer");
    let mut draft = Draft::new("at-native-old", "Native before epoch");
    draft.due_at = Some(due);
    let imported_created = "1960-01-01T00:00:00.123Z";
    let imported_due = "1960-01-02T00:00:00.456Z";
    let imported = format!(
        "{{\"id\":\"at-imported-old\",\"title\":\"Imported before epoch\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"{imported_created}\",\"due_at\":\"{imported_due}\"}}"
    );

    harness::block_on(async {
        store
            .create_item(actor.clone(), "native-old".into(), draft)
            .await
            .expect("create pre-epoch item");
        store
            .import(actor.clone(), imported, false)
            .await
            .expect("import pre-epoch item");
        let native = store
            .show(actor.clone(), "at-native-old".into())
            .await
            .expect("show native item")
            .expect("native item exists")
            .item;
        assert_eq!(native.created_at, created);
        assert_eq!(native.due_at, Some(due));
        let imported = store
            .show(actor.clone(), "at-imported-old".into())
            .await
            .expect("show imported item")
            .expect("imported item exists")
            .item;
        let imported_created_ms = chrono::DateTime::parse_from_rfc3339(imported_created)
            .expect("import created date")
            .timestamp_millis();
        let imported_due_ms = chrono::DateTime::parse_from_rfc3339(imported_due)
            .expect("import due date")
            .timestamp_millis();
        assert!(imported_created_ms < 0 && imported_due_ms < 0);
        assert_eq!(
            imported.created_at,
            std::time::UNIX_EPOCH - Duration::from_millis(imported_created_ms.unsigned_abs())
        );
        assert_eq!(
            imported.due_at,
            Some(std::time::UNIX_EPOCH - Duration::from_millis(imported_due_ms.unsigned_abs()))
        );

        let exported = store.export(actor).await.expect("export graph");
        let rows: Vec<serde_json::Value> = exported
            .lines()
            .map(|line| serde_json::from_str(line).expect("exported JSON row"))
            .collect();
        for (id, expected_created, expected_due) in [
            ("at-native-old", created, due),
            (
                "at-imported-old",
                imported.created_at,
                imported.due_at.expect("imported due"),
            ),
        ] {
            let row = rows
                .iter()
                .find(|row| row["id"] == id)
                .expect("exported item");
            for (field, expected) in [("created_at", expected_created), ("due_at", expected_due)] {
                let actual = chrono::DateTime::parse_from_rfc3339(
                    row[field].as_str().expect("exported date string"),
                )
                .expect("valid exported date")
                .timestamp_millis();
                let expected = -(expected
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect_err("pre-epoch expected date")
                    .duration()
                    .as_millis() as i64);
                assert_eq!(actual, expected, "{id} {field} changed on export");
            }
        }
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_waits_for_every_child_and_never_queues_messages_or_unresolved_compensation() {
    use store_contract::{Actor, Draft, EdgeKind, Finish, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-readiness-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let actor = Actor::human("owner");
    harness::block_on(async {
        for id in ["gate", "one", "two", "comp", "blocker"] {
            store
                .create_item(actor.clone(), format!("create-{id}"), Draft::new(id, id))
                .await
                .expect("test operation");
        }
        let mut message = Draft::new("message", "A message");
        message.kind = "message".into();
        store
            .create_item(actor.clone(), "create-message".into(), message)
            .await
            .expect("test operation");
        for target in ["one", "two"] {
            store
                .add_edge(
                    actor.clone(),
                    "gate".into(),
                    EdgeKind::WaitFor,
                    target.into(),
                )
                .await
                .expect("test operation");
        }
        store
            .add_edge(
                actor.clone(),
                "comp".into(),
                EdgeKind::ConditionalBlocks,
                "blocker".into(),
            )
            .await
            .expect("test operation");
        let ready = store
            .ready(actor.clone(), 100)
            .await
            .expect("test operation");
        assert!(
            !ready
                .iter()
                .any(|item| item.id == "gate" || item.id == "message" || item.id == "comp")
        );
        assert!(
            store
                .claim(
                    actor.clone(),
                    "worker".into(),
                    "message".into(),
                    Duration::from_secs(60),
                )
                .await
                .expect("message claim check")
                .is_none(),
            "a message must not enter the work queue through a direct claim"
        );
        for target in ["one", "two"] {
            let lease = store
                .claim(
                    actor.clone(),
                    "worker".into(),
                    target.into(),
                    Duration::from_secs(60),
                )
                .await
                .expect("test operation")
                .expect("test operation");
            store
                .finish(lease, Finish::Close)
                .await
                .expect("test operation");
            let ready = store
                .ready(actor.clone(), 100)
                .await
                .expect("test operation");
            assert_eq!(ready.iter().any(|item| item.id == "gate"), target == "two");
        }
        let lease = store
            .claim(
                actor.clone(),
                "worker".into(),
                "blocker".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("test operation")
            .expect("test operation");
        store
            .finish(lease, Finish::Close)
            .await
            .expect("test operation");
        assert!(
            !store
                .ready(actor, 100)
                .await
                .expect("test operation")
                .iter()
                .any(|item| item.id == "comp")
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_ready_and_claim_follow_a_blocker_chain_with_a_cycle() {
    use store_contract::{Actor, Draft, EdgeKind, Finish, GraphStore, Status};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-blocker-chain-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("graph store");
    let actor = Actor::human("owner");
    harness::block_on(async {
        const COUNT: usize = 16;
        for n in 0..COUNT {
            let id = format!("at-chain-{n:02}");
            store
                .create_item(actor.clone(), format!("create-{id}"), Draft::new(&id, &id))
                .await
                .expect("create chain item");
        }
        for n in 0..COUNT - 1 {
            store
                .add_edge(
                    actor.clone(),
                    format!("at-chain-{n:02}"),
                    EdgeKind::Blocks,
                    format!("at-chain-{:02}", n + 1),
                )
                .await
                .expect("add chain blocker");
        }
        store
            .add_edge(
                actor.clone(),
                "at-chain-15".into(),
                EdgeKind::Blocks,
                "at-chain-14".into(),
            )
            .await
            .expect("add cycle edge");
        assert!(
            store
                .ready(actor.clone(), COUNT)
                .await
                .expect("ready before close")
                .is_empty()
        );
        assert!(
            store
                .claim(
                    actor.clone(),
                    "worker".into(),
                    "at-chain-00".into(),
                    Duration::from_secs(60),
                )
                .await
                .expect("direct claim check")
                .is_none(),
            "direct claim ignored a blocker"
        );
        store
            .set_status(actor.clone(), "at-chain-15".into(), Status::Closed)
            .await
            .expect("human breaks cycle");
        for n in (0..COUNT - 1).rev() {
            let expected = format!("at-chain-{n:02}");
            let ready = store.ready(actor.clone(), COUNT).await.expect("ready item");
            assert_eq!(
                ready
                    .iter()
                    .map(|item| item.id.as_str())
                    .collect::<Vec<_>>(),
                [expected.as_str()]
            );
            let lease = store
                .claim_next(actor.clone(), "worker".into(), Duration::from_secs(60))
                .await
                .expect("claim next")
                .expect("one item became ready");
            assert_eq!(lease.item_id, expected);
            store
                .finish(lease, Finish::Close)
                .await
                .expect("close claimed item");
        }
        assert!(
            store
                .ready(actor, COUNT)
                .await
                .expect("ready after close")
                .is_empty()
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_import_refuses_undefined_dependency_kind_without_committing_items() {
    use store_contract::{Actor, GraphStore, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-undefined-edge-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph store");
    let actor = Actor::human("owner");
    let imported = [
        serde_json::json!({"id":"at-blocker","title":"Blocker","status":"open","priority":2,"issue_type":"task"}),
        serde_json::json!({"id":"at-work","title":"Work","status":"open","priority":2,"issue_type":"task","dependencies":[{"issue_id":"at-work","depends_on_id":"at-blocker","type":"future-blocks"}]}),
    ]
    .into_iter()
    .map(|row| row.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    harness::block_on(async {
        let before = store
            .export(actor.clone())
            .await
            .expect("export before import");
        let error = store
            .import(actor.clone(), imported, false)
            .await
            .expect_err("undefined edge kind must be refused");
        assert!(
            matches!(&error, StoreError::ImportRefused(problems) if problems.iter().any(|problem| problem.contains("unsupported type future-blocks"))),
            "unexpected import refusal: {error:?}"
        );
        assert_eq!(
            store.export(actor).await.expect("export after refusal"),
            before,
            "refused undefined edge kind must leave no committed item"
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[cfg(debug_assertions)]
#[test]
fn sqlite_projection_refresh_never_exposes_a_claimable_gap() {
    use store_contract::{Actor, Draft, EdgeKind};

    let path = std::env::temp_dir().join(format!(
        "atomic-graph-projection-race-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let setup = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("open setup graph store");
    harness::block_on(async {
        for id in ["at-blocked", "at-blocker"] {
            setup
                .create_item(
                    Actor::agent("setup"),
                    format!("create-{id}"),
                    Draft::new(id, id),
                )
                .await
                .expect("create item");
        }
        setup
            .add_edge(
                Actor::agent("setup"),
                "at-blocked".into(),
                EdgeKind::Blocks,
                "at-blocker".into(),
            )
            .await
            .expect("block item");
    });
    drop(setup);

    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let writer_store = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("open writer graph handle")
        .with_projection_pause(move || {
            paused_tx.send(()).expect("signal deleted projection");
            release_rx
                .lock()
                .expect("release receiver")
                .recv_timeout(Duration::from_secs(15))
                .expect("release projection refresh");
        });
    let claimant_store = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("open independent claimant graph handle");

    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            harness::block_on(writer_store.create_item(
                Actor::agent("writer"),
                "unrelated-write".into(),
                Draft::new("at-unrelated", "Unrelated"),
            ))
        });
        paused_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer did not reach projection gap");

        let (polled_tx, polled_rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let claimant = scope.spawn(move || {
            let pending = claimant_store.claim(
                Actor::agent("claimant"),
                "claimant".into(),
                "at-blocked".into(),
                Duration::from_secs(60),
            );
            let mut pending = std::pin::pin!(pending);
            let mut first_poll = Some(polled_tx);
            let result = harness::block_on(std::future::poll_fn(move |cx| {
                let result = pending.as_mut().poll(cx);
                if let Some(signal) = first_poll.take() {
                    signal.send(()).expect("signal claimant poll");
                }
                result
            }));
            answer_tx.send(result).expect("send claim result");
        });
        polled_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("claimant was not polled");
        let during_refresh = answer_rx.recv_timeout(Duration::from_millis(500));
        release_tx.send(()).expect("release writer");
        writer
            .join()
            .expect("writer thread")
            .expect("writer mutation");
        assert!(
            matches!(
                during_refresh,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "claim returned while projection rows were deleted: {during_refresh:?}"
        );
        let claimed = answer_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("claimant did not finish after commit")
            .expect("claim operation");
        assert!(claimed.is_none(), "blocked item was claimed after refresh");
        claimant.join().expect("claimant thread");
    });

    let reopened = atomic_graph::sqlite::SqliteStore::open(&path, std::time::SystemTime::now)
        .expect("reopen graph after race");
    let blocked = harness::block_on(reopened.show(Actor::agent("reader"), "at-blocked".into()))
        .expect("show blocked item")
        .expect("blocked item exists");
    assert_eq!(blocked.blocked_by, ["at-blocker"]);
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_export_includes_committed_changes_after_import() {
    use store_contract::{Actor, EdgeKind, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-export-live-{}.sqlite",
        std::process::id()
    ));
    let restored_path = path.with_extension("restored.sqlite");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&restored_path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let actor = Actor::human("owner");
    let input = concat!(
        r#"{"id":"at-a","title":"A","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","custom":{"keep":true}}"#,
        "\n",
        r#"{"id":"at-b","title":"B","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z"}"#,
        "\n"
    );
    harness::block_on(async {
        store
            .import(actor.clone(), input.into(), false)
            .await
            .expect("test operation");
        let lease = store
            .claim(
                actor.clone(),
                "worker".into(),
                "at-a".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("test operation")
            .expect("test operation");
        store
            .note(actor.clone(), lease, "note".into(), "new note".into())
            .await
            .expect("test operation");
        store
            .add_edge(
                actor.clone(),
                "at-a".into(),
                EdgeKind::Related,
                "at-b".into(),
            )
            .await
            .expect("test operation");
        let export = store
            .export(actor.clone())
            .await
            .expect("export changed graph");
        let first: serde_json::Value =
            serde_json::from_str(export.lines().next().expect("test operation"))
                .expect("test operation");
        assert_eq!(first["status"], "in_progress");
        assert_eq!(first["custom"]["keep"], true);
        assert_eq!(first["comments"][0]["text"], "new note");
        assert_eq!(first["dependencies"][0]["type"], "related");
        let restored = sqlite_authorized_for_tests(&restored_path, std::time::SystemTime::now)
            .expect("open restored graph");
        restored
            .import(actor.clone(), export, false)
            .await
            .expect("reimport changed graph");
        let original = store
            .show(actor.clone(), "at-a".into())
            .await
            .expect("show source")
            .expect("source item");
        let copy = restored
            .show(actor.clone(), "at-a".into())
            .await
            .expect("show copy")
            .expect("copied item");
        assert_eq!(copy.item.status, original.item.status);
        assert_eq!(copy.item.body, original.item.body);
        assert_eq!(copy.item.created_at, original.item.created_at);
        assert_eq!(copy.notes.len(), original.notes.len());
        assert_eq!(copy.notes[0].text, "new note");
        assert_eq!(copy.blocked_by, original.blocked_by);
        assert_eq!(
            restored
                .export(actor.clone())
                .await
                .expect("export restored graph"),
            store.export(actor).await.expect("export source graph"),
            "roundtrip lost the live edge, note, or unrepresented imported field"
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(restored_path);
}

#[test]
fn sqlite_claims_are_serialized_across_independent_connections() {
    use store_contract::{Actor, Draft, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-cross-connection-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let first =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    harness::block_on(first.create_item(
        Actor::agent("setup"),
        "setup".into(),
        Draft::new("at-race", "Race"),
    ))
    .expect("test operation");
    let second =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let start = std::sync::Arc::new(std::sync::Barrier::new(3));
    let racers = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                harness::block_on(store.claim(
                    Actor::agent("racer"),
                    format!("racer-{index}"),
                    "at-race".into(),
                    Duration::from_secs(60),
                ))
                .expect("test operation")
            })
        })
        .collect::<Vec<_>>();
    start.wait();
    let winners = racers
        .into_iter()
        .map(|racer| racer.join().expect("test operation"))
        .filter(Option::is_some)
        .count();
    assert_eq!(winners, 1);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_samples_lease_clock_when_the_transaction_starts() {
    use store_contract::{Actor, Draft, GraphStore};
    let path =
        std::env::temp_dir().join(format!("atomic-graph-clock-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let clock = Clock::new();
    let observed = clock.clone();
    let store = sqlite_authorized_for_tests(&path, move || observed.now()).expect("test operation");
    let actor = Actor::agent("worker");
    harness::block_on(store.create_item(
        actor.clone(),
        "setup".into(),
        Draft::new("at-clock", "Clock"),
    ))
    .expect("test operation");
    let pending = store.claim(
        actor.clone(),
        "first".into(),
        "at-clock".into(),
        Duration::from_secs(1),
    );
    clock.advance(Duration::from_secs(30));
    let lease = poll_after_writer_contention(&path, pending, &clock)
        .expect("test operation")
        .expect("test operation");
    assert_eq!(lease.epoch, 1);
    assert_lease_expiry_after(&path, "at-clock", clock.now());
    let second = harness::block_on(store.claim(
        actor,
        "second".into(),
        "at-clock".into(),
        Duration::from_secs(1),
    ))
    .expect("test operation");
    assert!(
        second.is_none(),
        "claim was committed already expired from a stale clock sample"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}

/// Keep a separate SQLite writer open while the graph future is first polled.
/// The future was constructed before the caller advanced its fake clock.
fn poll_after_writer_contention<F>(path: &std::path::Path, pending: F, clock: &Clock) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    use sqlx::Connection;
    let uri = format!("sqlite://{}", path.display());
    let mut writer =
        harness::block_on(sqlx::SqliteConnection::connect(&uri)).expect("open independent writer");
    harness::block_on(sqlx::query("BEGIN IMMEDIATE").execute(&mut writer))
        .expect("hold writer transaction");
    std::thread::scope(|scope| {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || {
            let mut pending = std::pin::pin!(pending);
            let mut first_poll = Some(started_tx);
            harness::block_on(std::future::poll_fn(move |cx| {
                let result = pending.as_mut().poll(cx);
                if let Some(signal) = first_poll.take() {
                    signal.send(()).expect("signal first graph poll");
                }
                result
            }))
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("graph future was polled");
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            !worker.is_finished(),
            "graph operation ignored writer contention"
        );
        // An implementation that samples time at first poll, before BEGIN
        // IMMEDIATE can enter, must still use the later transaction clock.
        clock.advance(Duration::from_secs(30));
        harness::block_on(sqlx::query("ROLLBACK").execute(&mut writer))
            .expect("release writer transaction");
        worker.join().expect("graph worker")
    })
}

fn assert_lease_expiry_after(path: &std::path::Path, item: &str, now: std::time::SystemTime) {
    use sqlx::Connection;
    let uri = format!("sqlite://{}", path.display());
    harness::block_on(async {
        let mut conn = sqlx::SqliteConnection::connect(&uri)
            .await
            .expect("open lease reader");
        let expires_at: i64 =
            sqlx::query_scalar("SELECT expires_at FROM atomic_graph_leases WHERE item_id=?")
                .bind(item)
                .fetch_one(&mut conn)
                .await
                .expect("read lease expiry");
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .expect("positive fake clock")
            .as_millis() as i64;
        assert_eq!(expires_at, now_ms + 1_000, "lease used a stale clock");
    });
}

#[test]
fn sqlite_claim_next_samples_clock_after_poll_and_writer_contention() {
    use store_contract::{Actor, Draft, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-claim-next-clock-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let clock = Clock::new();
    let observed = clock.clone();
    let store = sqlite_authorized_for_tests(&path, move || observed.now()).expect("graph store");
    let actor = Actor::agent("worker");
    harness::block_on(store.create_item(
        actor.clone(),
        "setup".into(),
        Draft::new("at-next-clock", "Clock"),
    ))
    .expect("create item");
    let pending = store.claim_next(actor.clone(), "first".into(), Duration::from_secs(1));
    clock.advance(Duration::from_secs(30));
    let lease = poll_after_writer_contention(&path, pending, &clock)
        .expect("claim next")
        .expect("ready item");
    assert_eq!(lease.item_id, "at-next-clock");
    assert_lease_expiry_after(&path, &lease.item_id, clock.now());
    assert!(
        harness::block_on(store.claim(
            actor,
            "second".into(),
            lease.item_id.clone(),
            Duration::from_secs(1),
        ))
        .expect("second claim")
        .is_none(),
        "second claimant took a newly committed lease"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_heartbeat_samples_clock_after_poll_and_writer_contention() {
    use store_contract::{Actor, Draft, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-heartbeat-clock-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let clock = Clock::new();
    let observed = clock.clone();
    let store = sqlite_authorized_for_tests(&path, move || observed.now()).expect("graph store");
    let actor = Actor::agent("worker");
    harness::block_on(store.create_item(
        actor.clone(),
        "setup".into(),
        Draft::new("at-heartbeat-clock", "Clock"),
    ))
    .expect("create item");
    let lease = harness::block_on(store.claim(
        actor.clone(),
        "first".into(),
        "at-heartbeat-clock".into(),
        Duration::from_secs(1),
    ))
    .expect("first claim")
    .expect("open item");
    let pending = store.heartbeat(lease.clone(), Duration::from_secs(1));
    clock.advance(Duration::from_secs(30));
    poll_after_writer_contention(&path, pending, &clock).expect("heartbeat");
    assert_lease_expiry_after(&path, &lease.item_id, clock.now());
    assert!(
        harness::block_on(store.claim(
            actor,
            "second".into(),
            lease.item_id,
            Duration::from_secs(1),
        ))
        .expect("second claim")
        .is_none(),
        "second claimant took a renewed lease"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_fifty_contenders_have_exactly_one_claim_winner() {
    use store_contract::{Actor, Draft, GraphStore};
    let path =
        std::env::temp_dir().join(format!("atomic-graph-fifty-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    harness::block_on(store.create_item(
        Actor::agent("setup"),
        "setup".into(),
        Draft::new("at-fifty", "Fifty"),
    ))
    .expect("test operation");
    let attempts = harness::race(&store, 50, |store, claimant| {
        Box::pin(async move {
            store
                .claim(
                    Actor::agent("racer"),
                    format!("claimant-{claimant}"),
                    "at-fifty".into(),
                    Duration::from_secs(60),
                )
                .await
        })
    });
    assert!(attempts.problems.is_empty(), "{:?}", attempts.problems);
    let winners = attempts
        .answers
        .into_iter()
        .flatten()
        .filter_map(|answer| answer.expect("claim result"))
        .count();
    assert_eq!(winners, 1);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_does_not_block_a_current_thread_tokio_executor_while_waiting_for_a_writer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use store_contract::{Actor, Draft, GraphStore};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-executor-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let waiting_store =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("test operation");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test operation");
    for id in ["at-writer", "at-wait"] {
        runtime
            .block_on(waiting_store.create_item(
                Actor::agent("setup"),
                format!("setup-{id}"),
                Draft::new(id, id),
            ))
            .expect("test operation");
    }

    // The clock runs inside the writer's BEGIN IMMEDIATE transaction. Keep
    // that graph handle's write lock until a timer on this same executor
    // releases it, while a second graph handle attempts another write.
    let writer_locked = std::sync::Arc::new(AtomicBool::new(false));
    let locked_in_clock = std::sync::Arc::clone(&writer_locked);
    let (release_writer, released) = std::sync::mpsc::channel();
    let released = std::sync::Mutex::new(released);
    let writer_store = sqlite_authorized_for_tests(&path, move || {
        locked_in_clock.store(true, Ordering::Release);
        let _ = released.lock().expect("release receiver").recv();
        std::time::SystemTime::now()
    })
    .expect("open independent writer graph handle");
    runtime.block_on(async {
        let writer = tokio::spawn(async move {
            writer_store
                .claim(
                    Actor::agent("writer"),
                    "writer".into(),
                    "at-writer".into(),
                    Duration::from_secs(60),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !writer_locked.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("writer did not take its graph transaction lock");

        let timer_fired = std::sync::Arc::new(AtomicBool::new(false));
        let timer_observed = std::sync::Arc::clone(&timer_fired);
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            timer_observed.store(true, Ordering::Release);
            release_writer.send(()).expect("release writer");
        });
        let waiting = tokio::time::timeout(
            Duration::from_secs(2),
            waiting_store.claim(
                Actor::agent("worker"),
                "worker".into(),
                "at-wait".into(),
                Duration::from_secs(60),
            ),
        )
        .await
        .expect("contended graph claim blocked the current-thread executor")
        .expect("contended graph claim failed");
        assert!(
            waiting.is_some(),
            "contended graph claim lost after writer released the lock"
        );
        assert!(
            timer_fired.load(Ordering::Acquire),
            "the same-runtime timer did not release the writer"
        );
        release.await.expect("timer task");
        writer
            .await
            .expect("writer task")
            .expect("writer graph claim")
            .expect("writer claim was empty");
    });
    drop(waiting_store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_export_keeps_current_closed_time_and_stamped_import_creation_time() {
    use store_contract::{Actor, GraphStore, Status};
    let first_path = std::env::temp_dir().join(format!(
        "atomic-graph-dates-a-{}.sqlite",
        std::process::id()
    ));
    let second_path = std::env::temp_dir().join(format!(
        "atomic-graph-dates-b-{}.sqlite",
        std::process::id()
    ));
    for path in [&first_path, &second_path] {
        let _ = std::fs::remove_file(path);
    }
    let clock = Clock::new();
    let observed = clock.clone();
    let first =
        sqlite_authorized_for_tests(&first_path, move || observed.now()).expect("open first graph");
    let human = Actor::human("owner");
    let input = concat!(
        r#"{"id":"at-closed","title":"Closed","status":"closed","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","closed_at":"2026-01-02T00:00:00Z"}"#,
        "\n",
        r#"{"id":"at-no-created","title":"Stamped","status":"open","priority":2,"issue_type":"task"}"#,
        "\n"
    );
    harness::block_on(async {
        first
            .import(human.clone(), input.into(), false)
            .await
            .expect("import dates");
        first
            .set_status(human.clone(), "at-closed".into(), Status::Open)
            .await
            .expect("reopen");
        clock.advance(Duration::from_secs(15));
        first
            .set_status(human.clone(), "at-closed".into(), Status::Closed)
            .await
            .expect("first new close");
        let first_new_close = clock.now();
        first
            .set_status(human.clone(), "at-closed".into(), Status::Open)
            .await
            .expect("reopen after first new close");
        clock.advance(Duration::from_secs(20));
        first
            .set_status(human.clone(), "at-closed".into(), Status::Closed)
            .await
            .expect("latest close");
        let latest_close = clock.now();
        assert_ne!(first_new_close, latest_close);
        let export = first.export(human.clone()).await.expect("export dates");
        let rows: Vec<serde_json::Value> = export
            .lines()
            .map(|line| serde_json::from_str(line).expect("export JSON"))
            .collect();
        let closed = rows
            .iter()
            .find(|row| row["id"] == "at-closed")
            .expect("closed row");
        assert_eq!(closed["created_at"], "2026-01-01T00:00:00Z");
        assert_ne!(closed["closed_at"], "2026-01-02T00:00:00Z");
        let current = first
            .show(human.clone(), "at-closed".into())
            .await
            .expect("show current closed item")
            .expect("current closed item");
        assert_eq!(current.item.status, Status::Closed);
        assert_eq!(current.item.closed_at, Some(latest_close));
        let stamped = rows
            .iter()
            .find(|row| row["id"] == "at-no-created")
            .expect("stamped row");
        assert!(
            stamped["created_at"].is_string(),
            "import-stamped creation time must export"
        );
        let second = sqlite_authorized_for_tests(&second_path, std::time::SystemTime::now)
            .expect("open second graph");
        second
            .import(human.clone(), export, false)
            .await
            .expect("restore export");
        let before = first
            .show(human.clone(), "at-no-created".into())
            .await
            .expect("show first")
            .expect("first item");
        let after = second
            .show(human.clone(), "at-no-created".into())
            .await
            .expect("show second")
            .expect("second item");
        assert_eq!(before.item.created_at, after.item.created_at);
        let restored_closed = second
            .show(human, "at-closed".into())
            .await
            .expect("show restored closed item")
            .expect("restored closed item");
        assert_eq!(restored_closed.item.created_at, current.item.created_at);
        assert_eq!(restored_closed.item.closed_at, current.item.closed_at);
        assert_eq!(restored_closed.item.closed_at, Some(latest_close));
    });
    drop(first);
    for path in [first_path, second_path] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn sqlite_exports_beads_memory_records_without_promoting_imported_trust() {
    use store_contract::{Actor, GraphStore};
    let first_path = std::env::temp_dir().join(format!(
        "atomic-graph-memory-a-{}.sqlite",
        std::process::id()
    ));
    let second_path = std::env::temp_dir().join(format!(
        "atomic-graph-memory-b-{}.sqlite",
        std::process::id()
    ));
    for path in [&first_path, &second_path] {
        let _ = std::fs::remove_file(path);
    }
    let first = sqlite_authorized_for_tests(&first_path, std::time::SystemTime::now)
        .expect("open first graph");
    let second = sqlite_authorized_for_tests(&second_path, std::time::SystemTime::now)
        .expect("open second graph");
    let human = Actor::human("owner");
    let agent = Actor::agent("atom");
    harness::block_on(async {
        first
            .remember(
                human.clone(),
                "h1".into(),
                "human-key".into(),
                "human value".into(),
            )
            .await
            .expect("human memory");
        first
            .import(
                human.clone(),
                r#"{"_type":"memory","key":"external-key","value":"external value"}"#.into(),
                false,
            )
            .await
            .expect("external memory");
        let export = first
            .export(human.clone())
            .await
            .expect("memory-only export");
        let rows: Vec<serde_json::Value> = export
            .lines()
            .map(|line| serde_json::from_str(line).expect("memory JSON"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row["_type"] == "memory"
            && row["key"].is_string()
            && row["value"].is_string()));
        // An external record may claim human trust in extra fields. Import still forces external.
        let forged = r#"{"_type":"memory","key":"forged","value":"outside","trust":"human"}"#;
        second
            .import(agent.clone(), format!("{export}{forged}\n"), false)
            .await
            .expect("import memories");
        assert!(
            second
                .prompt_memories(agent.clone(), 10)
                .await
                .expect("prompt memories")
                .is_empty()
        );
        let restored = second
            .export(agent)
            .await
            .expect("export restored memories");
        let restored_values: std::collections::BTreeMap<String, String> = restored
            .lines()
            .map(|line| {
                let row: serde_json::Value =
                    serde_json::from_str(line).expect("restored memory JSON");
                assert_eq!(row["_type"], "memory");
                (
                    row["key"].as_str().expect("memory key").to_owned(),
                    row["value"].as_str().expect("memory value").to_owned(),
                )
            })
            .collect();
        assert_eq!(
            restored_values,
            std::collections::BTreeMap::from([
                ("human-key".to_owned(), "human value".to_owned()),
                ("external-key".to_owned(), "external value".to_owned()),
                ("forged".to_owned(), "outside".to_owned()),
            ]),
            "memory values must survive export and reimport exactly"
        );
    });
    drop(first);
    drop(second);
    for path in [first_path, second_path] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn sqlite_native_pinned_uses_marked_beads_metadata_and_generic_pinned_is_refused() {
    use store_contract::{Actor, Draft, GraphStore, Status, StoreError, Trust};
    let first_path = std::env::temp_dir().join(format!(
        "atomic-graph-pinned-a-{}.sqlite",
        std::process::id()
    ));
    let second_path = std::env::temp_dir().join(format!(
        "atomic-graph-pinned-b-{}.sqlite",
        std::process::id()
    ));
    for path in [&first_path, &second_path] {
        let _ = std::fs::remove_file(path);
    }
    let first = sqlite_authorized_for_tests(&first_path, std::time::SystemTime::now)
        .expect("open first graph");
    let second = sqlite_authorized_for_tests(&second_path, std::time::SystemTime::now)
        .expect("open second graph");
    let actor = Actor::human("owner");
    harness::block_on(async {
        let mut draft = Draft::new("at-pinned", "Pinned thread");
        draft.status = Status::Pinned;
        first
            .create_item(actor.clone(), "create-pinned".into(), draft)
            .await
            .expect("create pinned");
        let export = first.export(actor.clone()).await.expect("export pinned");
        let row: serde_json::Value =
            serde_json::from_str(export.lines().next().expect("one line")).expect("pinned JSON");
        assert_eq!(row["status"], "open");
        assert_eq!(row["pinned"], true);
        assert_eq!(row["metadata"]["atomic_graph"]["schema_version"], 1);
        assert_eq!(row["metadata"]["atomic_graph"]["native_status"], "pinned");
        second
            .import(actor.clone(), export, false)
            .await
            .expect("reimport native pinned");
        let shown = second
            .show(actor.clone(), "at-pinned".into())
            .await
            .expect("show pinned")
            .expect("pinned item");
        assert_eq!(shown.item.status, Status::Pinned);
        assert_eq!(shown.item.trust, Trust::External);
        let generic = r#"{"id":"at-generic","title":"Generic","status":"open","pinned":true}"#;
        assert!(matches!(
            second.import(actor.clone(), generic.into(), false).await,
            Err(StoreError::ImportRefused(_))
        ));
        let string_flag = r#"{"id":"at-string","title":"String","status":"open","pinned":"true"}"#;
        assert!(matches!(
            second.import(actor, string_flag.into(), false).await,
            Err(StoreError::ImportRefused(_))
        ));
    });
    drop(first);
    drop(second);
    for path in [first_path, second_path] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn sqlite_request_ids_bind_operation_and_target() {
    use sqlx::Connection;
    use store_contract::{Actor, Draft, GraphStore, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-requests-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let actor = Actor::agent("atom");
    harness::block_on(async {
        let first = store
            .create_item(actor.clone(), "same".into(), Draft::new("at-one", "One"))
            .await
            .expect("create one");
        assert_eq!(
            store
                .create_item(actor.clone(), "same".into(), Draft::new("at-one", "One"))
                .await
                .expect("retry one"),
            first
        );
        assert!(matches!(
            store
                .create_item(
                    actor.clone(),
                    "same".into(),
                    Draft::new("at-one", "Changed title")
                )
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        assert!(matches!(
            store
                .create_item(actor.clone(), "same".into(), Draft::new("at-two", "Two"))
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        let second = store
            .create_item(
                actor.clone(),
                "create-two".into(),
                Draft::new("at-two", "Two"),
            )
            .await
            .expect("create two");
        let mut legacy = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("legacy request database");
        sqlx::query(
            "UPDATE atomic_graph_requests SET value='at-two' WHERE request_id='create-two'",
        )
        .execute(&mut legacy)
        .await
        .expect("simulate a pre-fingerprint create request");
        assert_eq!(
            store
                .create_item(
                    actor.clone(),
                    "create-two".into(),
                    Draft::new("at-two", "Two")
                )
                .await
                .expect("legacy request replays"),
            second
        );
        let one = store
            .claim(
                actor.clone(),
                "worker".into(),
                "at-one".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim one")
            .expect("lease one");
        let two = store
            .claim(
                actor.clone(),
                "worker".into(),
                "at-two".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim two")
            .expect("lease two");
        assert!(matches!(
            store
                .note(actor.clone(), one.clone(), "same".into(), "wrong op".into())
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        store
            .note(
                actor.clone(),
                one.clone(),
                "note-one".into(),
                "one note".into(),
            )
            .await
            .expect("note one");
        assert!(matches!(
            store
                .create_item(
                    actor.clone(),
                    "note-one".into(),
                    Draft::new("at-one", "One")
                )
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        assert!(matches!(
            store
                .remember(
                    actor.clone(),
                    "note-one".into(),
                    "at-one".into(),
                    "wrong op".into()
                )
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        assert!(matches!(
            store
                .note(actor.clone(), two, "note-one".into(), "two note".into())
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        assert!(matches!(
            store
                .remember(
                    actor.clone(),
                    "same".into(),
                    "memory".into(),
                    "wrong op".into()
                )
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
        store
            .remember(
                actor.clone(),
                "memory-one".into(),
                "key-one".into(),
                "value".into(),
            )
            .await
            .expect("remember one");
        assert!(matches!(
            store
                .remember(actor, "memory-one".into(), "key-two".into(), "value".into())
                .await,
            Err(StoreError::RequestConflict { .. })
        ));
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_admin_status_changes_invalidate_earlier_close_replay() {
    use store_contract::{Actor, Draft, Finish, GraphStore, Status, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-admin-replay-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let human = Actor::human("owner");
    let agent = Actor::agent("atom");
    harness::block_on(async {
        store
            .create_item(
                human.clone(),
                "create".into(),
                Draft::new("at-replay", "Replay"),
            )
            .await
            .expect("create");
        let first = store
            .claim(
                agent.clone(),
                "A".into(),
                "at-replay".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim A")
            .expect("lease A");
        store
            .finish(first.clone(), Finish::Close)
            .await
            .expect("A closes");
        human_status(&store, human.clone(), Status::Open).await;
        let second = store
            .claim(
                agent.clone(),
                "B".into(),
                "at-replay".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim B")
            .expect("lease B");
        human_status(&store, human.clone(), Status::Closed).await;
        assert!(matches!(
            store.finish(first, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        assert!(matches!(
            store.finish(second, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        assert_eq!(
            store
                .show(human, "at-replay".into())
                .await
                .expect("show")
                .expect("item")
                .item
                .status,
            Status::Closed
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
    async fn human_status(store: &atomic_graph::sqlite::SqliteStore, actor: Actor, status: Status) {
        store
            .set_status(actor, "at-replay".into(), status)
            .await
            .expect("human status change");
    }
}

#[test]
fn sqlite_rejects_zero_and_overflowing_lease_durations_without_changing_holder() {
    use store_contract::{Actor, Draft, GraphStore, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-duration-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let actor = Actor::agent("atom");
    harness::block_on(async {
        store
            .create_item(
                actor.clone(),
                "create".into(),
                Draft::new("at-duration", "Duration"),
            )
            .await
            .expect("create");
        for invalid in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_secs(u64::MAX),
        ] {
            assert!(matches!(
                store
                    .claim(
                        actor.clone(),
                        "worker".into(),
                        "at-duration".into(),
                        invalid
                    )
                    .await,
                Err(StoreError::InvalidDuration)
            ));
            assert!(matches!(
                store
                    .claim_next(actor.clone(), "worker".into(), invalid)
                    .await,
                Err(StoreError::InvalidDuration)
            ));
        }
        let lease = store
            .claim(
                actor.clone(),
                "worker".into(),
                "at-duration".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim")
            .expect("lease");
        for invalid in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_secs(u64::MAX),
        ] {
            assert!(matches!(
                store.heartbeat(lease.clone(), invalid).await,
                Err(StoreError::InvalidDuration)
            ));
        }
        assert_eq!(
            store
                .show(actor, "at-duration".into())
                .await
                .expect("show")
                .expect("item")
                .holder
                .expect("holder")
                .epoch,
            lease.epoch
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_conditional_compensation_requires_persisted_terminal_failure() {
    use store_contract::{Actor, Draft, EdgeKind, Finish, GraphStore, Status, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-conditional-outcome-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let human = Actor::human("owner");
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    harness::block_on(async {
        for id in [
            "success",
            "failure",
            "unknown",
            "on-success",
            "on-failure",
            "on-unknown",
        ] {
            store
                .create_item(human.clone(), format!("create-{id}"), Draft::new(id, id))
                .await
                .expect("create item");
        }
        for (compensation, target) in [
            ("on-success", "success"),
            ("on-failure", "failure"),
            ("on-unknown", "unknown"),
        ] {
            store
                .add_edge(
                    human.clone(),
                    compensation.into(),
                    EdgeKind::ConditionalBlocks,
                    target.into(),
                )
                .await
                .expect("add conditional edge");
        }
        let success = store
            .claim(
                human.clone(),
                "worker".into(),
                "success".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim successful item")
            .expect("success lease");
        store
            .finish(success, Finish::Close)
            .await
            .expect("successful close");
        let failure = store
            .claim(
                human.clone(),
                "worker".into(),
                "failure".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim failed item")
            .expect("failure lease");
        store
            .finish(failure.clone(), Finish::Fail)
            .await
            .expect("terminal failure");
        store
            .finish(failure.clone(), Finish::Fail)
            .await
            .expect("idempotent failure replay");
        assert!(matches!(
            store.finish(failure, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        store
            .set_status(human.clone(), "unknown".into(), Status::Closed)
            .await
            .expect("administrative close has unknown outcome");
        let ready = store.ready(human.clone(), 20).await.expect("ready");
        assert!(ready.iter().any(|item| item.id == "on-failure"));
        assert!(
            !ready
                .iter()
                .any(|item| item.id == "on-success" || item.id == "on-unknown")
        );
    });
    drop(store);
    let reopened =
        sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("reopen graph");
    harness::block_on(async {
        let ready = reopened
            .ready(human.clone(), 20)
            .await
            .expect("ready after reopen");
        assert!(ready.iter().any(|item| item.id == "on-failure"));
        assert!(
            !ready
                .iter()
                .any(|item| item.id == "on-success" || item.id == "on-unknown")
        );
        let restored_path = path.with_extension("restored.sqlite");
        let _ = std::fs::remove_file(&restored_path);
        let restored = sqlite_authorized_for_tests(&restored_path, std::time::SystemTime::now)
            .expect("restored graph");
        let export = reopened
            .export(human.clone())
            .await
            .expect("export closed prerequisites");
        restored
            .import(human.clone(), export, false)
            .await
            .expect("restore closed prerequisites");
        let restored_ready = restored
            .ready(human.clone(), 20)
            .await
            .expect("ready after import");
        assert_eq!(
            restored_ready.iter().map(|i| &i.id).collect::<Vec<_>>(),
            ready.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
        for (status, version, outcome) in [
            ("open", 1, "failure"),
            ("closed", 2, "failure"),
            ("closed", 1, "retry"),
        ] {
            let bad = serde_json::json!({"id":"bad","title":"bad","status":status,
                "metadata":{"atomic_graph":{"schema_version":version,"terminal_outcome":outcome}}});
            assert!(
                matches!(restored.import(human.clone(), bad.to_string(), false).await,
                Err(StoreError::ImportRefused(errors)) if errors.iter().any(|e| e.contains("invalid native terminal outcome")))
            );
        }
        restored
            .set_status(human.clone(), "failure".into(), Status::Open)
            .await
            .expect("reopen imported failure");
        let export = restored
            .export(human.clone())
            .await
            .expect("export reopened item");
        let entries: Vec<serde_json::Value> = export
            .lines()
            .map(|s| serde_json::from_str(s).expect("exported JSON"))
            .collect();
        let item = |id: &str| {
            entries
                .iter()
                .find(|v| v["id"] == id)
                .expect("exported item")
        };
        assert!(
            item("failure")
                .pointer("/metadata/atomic_graph/terminal_outcome")
                .is_none()
        );
        assert_eq!(
            item("success")
                .pointer("/metadata/atomic_graph/terminal_outcome")
                .and_then(|v| v.as_str()),
            Some("success")
        );
        assert!(
            item("unknown")
                .pointer("/metadata/atomic_graph/terminal_outcome")
                .is_none()
        );
        drop(restored);
        std::fs::remove_file(restored_path).expect("remove restored database");
        assert!(
            reopened
                .claim(
                    human,
                    "compensator".into(),
                    "on-failure".into(),
                    Duration::from_secs(60)
                )
                .await
                .expect("claim compensation")
                .is_some()
        );
    });
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_human_admin_close_refuses_required_policy_and_fenced_close_succeeds_with_evidence() {
    use store_contract::{Actor, Draft, Evidence, Finish, GraphStore, Status, StoreError};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-admin-close-policy-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let human = Actor::human("owner");
    harness::block_on(async {
        store
            .set_close_policy(human.clone(), vec![Evidence::Commit])
            .await
            .expect("require commit");
        store
            .create_item(
                human.clone(),
                "create-work".into(),
                Draft::new("at-work", "Work"),
            )
            .await
            .expect("create work");
        assert!(matches!(
            store.set_status(human.clone(), "at-work".into(), Status::Closed).await,
            Err(StoreError::NeedsEvidence { item_id, required }) if item_id == "at-work" && required == "commit"
        ));
        let before = store
            .show(human.clone(), "at-work".into())
            .await
            .expect("show")
            .expect("item");
        assert_eq!(before.item.status, Status::Open);
        assert!(
            before.holder.is_none(),
            "refused admin close wrote no lease"
        );
        assert!(
            before.item.closed_at.is_none(),
            "refused close wrote no timestamp"
        );
        let lease = store
            .claim(
                human.clone(),
                "worker".into(),
                "at-work".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim")
            .expect("lease");
        assert!(matches!(store.finish(lease.clone(), Finish::Close).await,
            Err(StoreError::NeedsEvidence { item_id, required }) if item_id == "at-work" && required == "commit"));
        store
            .link_commit(lease.clone(), "abc123".into())
            .await
            .expect("link real commit evidence");
        store
            .finish(lease, Finish::Close)
            .await
            .expect("fenced evidenced close");
        store
            .create_item(
                human.clone(),
                "create-admin".into(),
                Draft::new("at-admin", "Admin"),
            )
            .await
            .expect("create admin-close item");
        let admin_lease = store
            .claim(
                human.clone(),
                "worker".into(),
                "at-admin".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim admin item")
            .expect("admin lease");
        store
            .link_commit(admin_lease.clone(), "admin-commit".into())
            .await
            .expect("link admin commit evidence");
        store
            .set_status(human.clone(), "at-admin".into(), Status::Closed)
            .await
            .expect("evidenced administrative close");
        assert!(matches!(
            store.finish(admin_lease, Finish::Close).await,
            Err(StoreError::Fenced { .. })
        ));
        store
            .create_item(
                human.clone(),
                "create-verified".into(),
                Draft::new("at-verified", "Verified"),
            )
            .await
            .expect("create verified item");
        store
            .set_item_close_policy(
                human.clone(),
                "at-verified".into(),
                vec![Evidence::VerifiedCommit],
                Some("refs/heads/main".into()),
            )
            .await
            .expect("require verified commit on a named branch");
        let verified_lease = store
            .claim(
                human.clone(),
                "worker".into(),
                "at-verified".into(),
                Duration::from_secs(60),
            )
            .await
            .expect("claim verified item")
            .expect("verified lease");
        store
            .link_commit(verified_lease.clone(), "unverified".into())
            .await
            .expect("link unverified commit");
        assert!(
            matches!(store.set_status(human.clone(), "at-verified".into(), Status::Closed).await,
            Err(StoreError::NeedsEvidence { item_id, required }) if item_id == "at-verified" && required == "verified_commit")
        );
        store
            .link_commit(verified_lease.clone(), "verified".into())
            .await
            .expect("a second model-facing link is still unverified");
        assert!(matches!(
            store.set_status(human.clone(), "at-verified".into(), Status::Closed).await,
            Err(StoreError::NeedsEvidence { item_id, required }) if item_id == "at-verified" && required == "verified_commit"
        ));
        assert_eq!(
            store
                .show(human.clone(), "at-verified".into())
                .await
                .expect("show")
                .expect("item")
                .item
                .status,
            Status::InProgress,
        );
        assert_eq!(
            store
                .show(human, "at-work".into())
                .await
                .expect("show")
                .expect("item")
                .item
                .status,
            Status::Closed
        );
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_preclosed_draft_refuses_without_item_or_request_write_but_historical_import_works() {
    use store_contract::{Actor, Draft, Evidence, GraphStore, Status, StoreError, Trust};
    let path = std::env::temp_dir().join(format!(
        "atomic-graph-preclosed-draft-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = sqlite_authorized_for_tests(&path, std::time::SystemTime::now).expect("open graph");
    let human = Actor::human("owner");
    harness::block_on(async {
        store
            .set_close_policy(human.clone(), vec![Evidence::Commit])
            .await
            .expect("require commit");
        let mut draft = Draft::new("at-direct", "Direct preclosed");
        draft.status = Status::Closed;
        assert!(
            matches!(store.create_item(human.clone(), "same-request".into(), draft).await,
            Err(StoreError::CannotCreateClosed { item_id }) if item_id == "at-direct")
        );
        assert!(
            store
                .all_items(human.clone())
                .await
                .expect("all items")
                .is_empty(),
            "refusal wrote no item"
        );
        assert!(
            store
                .show(human.clone(), "at-direct".into())
                .await
                .expect("show")
                .is_none()
        );
        let created = store
            .create_item(
                human.clone(),
                "same-request".into(),
                Draft::new("at-direct", "Open instead"),
            )
            .await
            .expect("refused request ID was not consumed");
        assert_eq!(created.status, Status::Open);
        let historical =
            "{\"id\":\"at-historical\",\"title\":\"Historical\",\"status\":\"closed\"}\n";
        store
            .import(human.clone(), historical.into(), false)
            .await
            .expect("external history import");
        let imported = store
            .show(human, "at-historical".into())
            .await
            .expect("show import")
            .expect("historical item");
        assert_eq!(imported.item.status, Status::Closed);
        assert_eq!(imported.item.trust, Trust::External);
    });
    drop(store);
    let _ = std::fs::remove_file(path);
}
