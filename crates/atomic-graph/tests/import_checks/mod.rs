// SPDX-License-Identifier: Apache-2.0

//! Importing beads' export, ported from Errai's `errai-tracker` import tests.
//!
//! Ported on 2026-11-23 from `crates/errai-tracker/tests/import.rs` in the
//! owner-approved Errai reference checkout. Errai read its fixture from
//! `tests/tracker/bd-synthetic.jsonl` and asserted against its Postgres tables
//! through SQL; here the fixture is in this file and everything is read back
//! through the store API:
//!
//! | Errai | Here |
//! | --- | --- |
//! | `include_str!("../../../tests/tracker/bd-synthetic.jsonl")` | [`SYNTHETIC`] below, counted by the same independent counter |
//! | `SELECT … FROM work_item ORDER BY id` | `all_items`, and `show` per item |
//! | `SELECT … FROM work_event WHERE kind = 'note'` | `Show::notes`, each with its author and trust |
//! | `payload->'kept'` | `Report::kept`, counted by field name |
//! | `ImportError::Refused` / `ImportError::DryRun` | `StoreError::ImportRefused` / `StoreError::ImportDryRun` |
//! | `report.left_as_is` prose strings | [`LeftAlone`], so the reason is answered rather than string-matched |
//! | a second tenant seeing none of it | deferred with tenancy, as in the lease checks (M7) |
//! | `ERRAI_TEST_BEADS_EXPORTS`, real exports | pending: M5 acceptance asks for a differential run against a pinned `bd`, which needs that pin. It is a row in the conformance target's pending list, not a check that quietly reads nothing |
//!
//! The fixture is written here rather than copied, because the two projects
//! differ in one place these checks care about: beads marks a pinned item with
//! a `pinned` *flag*, so a real export never carries `pinned` as a status.
//! Errai could only reach that case by tampering with the text; this fixture
//! carries both halves deliberately — an unknown status to refuse, and a
//! `pinned` status to refuse — and the checks say which is which.

use crate::harness::Clock;
use crate::harness::Run;
use crate::harness::atom;
use crate::store_contract::Draft;
use crate::store_contract::GraphStore;
use crate::store_contract::LeftAlone;
use crate::store_contract::Report;
use crate::store_contract::Status;
use crate::store_contract::StoreError;
use crate::store_contract::Trust;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

/// A beads-shaped export, seven items deep, with the things an importer gets
/// wrong: a closed item, an in-progress item, a deferred one, a blocked one
/// whose blocker is the in-progress item, an epic with a child, notes, two
/// comments with different authors, a token in a note, fields with no column of
/// their own, and a relation that must not become a blocker.
const SYNTHETIC: &str = concat!(
    r#"{"id":"syn-epic","title":"Synthetic epic","description":"A synthetic epic.\n\n## Design\n\nSynthetic design.","status":"open","priority":1,"issue_type":"epic","created_at":"2026-01-01T00:00:00Z","labels":["phase","synthetic"],"external_ref":"gh-7"}"#,
    "\n",
    r#"{"id":"syn-a","title":"Synthetic child, blocked by one in progress","description":"Synthetic child.","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-02T00:00:00Z","parent_issue":"syn-epic","dependencies":[{"issue_id":"syn-a","depends_on_id":"syn-c","type":"blocks"}]}"#,
    "\n",
    r#"{"id":"syn-b","title":"Synthetic closed","description":"Synthetic closed.","status":"closed","priority":2,"issue_type":"task","created_at":"2026-01-03T00:00:00Z","closed_at":"2026-01-04T00:00:00Z","notes":"First synthetic note.\nSecond synthetic note.","acceptance_criteria":"One\nTwo"}"#,
    "\n",
    r#"{"id":"syn-c","title":"Synthetic in progress","description":"Synthetic in progress.","status":"in_progress","priority":1,"issue_type":"feature","created_at":"2026-01-05T00:00:00Z","dependencies":[{"issue_id":"syn-c","depends_on_id":"syn-a","type":"related"},{"issue_id":"syn-c","depends_on_id":"syn-b","type":"discovered-from"}]}"#,
    "\n",
    r#"{"id":"syn-d","title":"Synthetic deferred","description":"Synthetic deferred, with a token.","status":"deferred","priority":4,"issue_type":"chore","created_at":"2026-01-06T00:00:00Z","external_ref":"gh-0","notes":"ghp_abcdefghij"}"#,
    "\n",
    r#"{"id":"syn-e","title":"Synthetic blocked","description":"Synthetic blocked.","status":"blocked","priority":2,"issue_type":"bug","created_at":"2026-01-07T00:00:00Z","dependencies":[{"issue_id":"syn-e","depends_on_id":"syn-c","type":"blocks"}]}"#,
    "\n",
    r#"{"id":"syn-f","title":"Synthetic open","description":"Synthetic open.","status":"open","priority":2,"issue_type":"bug","created_at":"2026-01-08T00:00:00Z","dependencies":[{"issue_id":"syn-f","depends_on_id":"syn-b","type":"blocks"}],"comments":[{"author":"Synthetic Person","created_at":"2026-01-08T00:10:00Z","text":"A synthetic comment."},{"author":"Synthetic Other","created_at":"2026-01-08T00:20:00Z","text":"Another synthetic comment."}]}"#,
    "\n",
);

/// What the file holds, counted from its text and not from the importer's own
/// reading of it — Errai's `expected`, kept as the check that does not trust the
/// code under test.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    items: usize,
    blocking: usize,
    parents: usize,
    notes: usize,
    relations: usize,
}

/// One line of the file, as the counter reads it.
#[derive(Debug)]
struct Line {
    id: String,
    status: String,
    parent: bool,
    notes: usize,
    /// What blocks this line, and the kind of every dependency it names.
    blockers: BTreeSet<String>,
    kinds: Vec<String>,
}

/// The value of `"key":"value"` in a line, or `None` if the line has no such
/// key. Enough of a reader for a fixture this suite owns, written without a
/// JSON dependency: the graph crate has none, and `--locked` will not gain one
/// here (see the `harness` module).
fn value(line: &str, key: &str) -> Option<String> {
    let needle = format!(r#""{key}":""#);
    let rest = line.split(&needle).nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// Every `"key":"value"` a line holds, in order.
fn values(line: &str, key: &str) -> Vec<String> {
    let needle = format!(r#""{key}":""#);
    line.split(&needle)
        .skip(1)
        .filter_map(|rest| rest.split('"').next().map(str::to_string))
        .collect()
}

/// Read one line. A line the counter cannot read is a bug in the fixture, so
/// this panics with the line rather than counting less than the file holds.
fn read(line: &str) -> Line {
    let id =
        value(line, "id").unwrap_or_else(|| panic!("the fixture has a line with no id: {line}"));
    let status = value(line, "status").unwrap_or_else(|| panic!("{id} has no status: {line}"));
    // Each dependency is a `depends_on_id` followed by its `type`, and nothing
    // else in the fixture uses either key.
    let mut blockers = BTreeSet::new();
    let mut kinds = Vec::new();
    for target in values(line, "depends_on_id") {
        let kind = values(line, "type")
            .get(kinds.len())
            .cloned()
            .unwrap_or_else(|| panic!("{id} has a dependency with no kind: {line}"));
        kinds.push(kind.clone());
        if kind == "blocks" {
            blockers.insert(target);
        }
    }
    let notes = usize::from(
        value(line, "notes").is_some_and(|notes| !notes.trim().is_empty() && notes != "\\n"),
    ) + values(line, "author").len();
    Line {
        id,
        status,
        parent: value(line, "parent_issue").is_some(),
        notes,
        blockers,
        kinds,
    }
}

/// Count the file, and the ids that should come out `ready` afterwards: an
/// `open` item whose every blocking dependency is closed.
fn expected(input: &str) -> (Counts, Vec<String>) {
    let lines: Vec<Line> = input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(read)
        .collect();
    let mut counts = Counts {
        items: lines.len(),
        ..Counts::default()
    };
    for line in &lines {
        counts.blocking += line.blockers.len();
        counts.notes += line.notes;
        counts.parents += usize::from(line.parent);
        counts.relations += line
            .kinds
            .iter()
            .filter(|kind| **kind != "blocks" && **kind != "parent-child")
            .count();
    }
    let status: BTreeMap<String, String> = lines
        .iter()
        .map(|line| (line.id.clone(), line.status.clone()))
        .collect();
    let mut ready: Vec<String> = lines
        .iter()
        .filter(|line| {
            line.status == "open"
                && line
                    .blockers
                    .iter()
                    .all(|blocker| status.get(blocker).map(String::as_str) == Some("closed"))
        })
        .map(|line| line.id.clone())
        .collect();
    ready.sort();
    (counts, ready)
}

/// What a whole store holds, so a check can say an import changed nothing:
/// Errai's `snapshot`, built from what the API returns instead of from tables.
async fn snapshot<S: GraphStore>(run: &mut Run, store: &S) -> Option<Vec<String>> {
    let items = run.ok("all_items", || store.all_items(atom())).await?;
    let mut lines = Vec::new();
    for item in &items {
        lines.push(format!(
            "item {} {} {} parent {} closed {}",
            item.id,
            item.status,
            item.trust,
            item.parent.clone().unwrap_or_else(|| "-".to_string()),
            item.closed_at.is_some(),
        ));
        let shown = run
            .some(&format!("{} in the snapshot", item.id), || {
                store.show(atom(), item.id.clone())
            })
            .await?;
        for note in shown.notes {
            lines.push(format!("note {} {} {}", item.id, note.author, note.text));
        }
        for blocker in shown.blocked_by {
            lines.push(format!("blocked {} {}", item.id, blocker));
        }
    }
    lines.sort();
    Some(lines)
}

/// The report a first import of the file should answer with.
fn want_report(counts: &Counts, ids: &[String]) -> Report {
    Report {
        items: counts.items,
        added: ids.to_vec(),
        unchanged: Vec::new(),
        left_as_is: Vec::new(),
        dependencies_added: counts.blocking,
        parents_set: counts.parents,
        notes_added: counts.notes,
        in_progress: 1,
        external: counts.items,
        kept: BTreeMap::from([
            ("labels".to_string(), 1),
            ("relations".to_string(), 1),
            ("external_ref".to_string(), 2),
            ("acceptance_criteria".to_string(), 1),
        ]),
    }
}

/// Port of `a_synthetic_export_imports_as_external_text_with_its_shape`.
pub async fn a_synthetic_export_imports_with_its_shape<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    let (counts, want_ready) = expected(&input);
    let ids = want_ids(&input);
    let Some(report) = run
        .ok("import", || store.import(atom(), input, false))
        .await
    else {
        return run.finish();
    };
    let want = want_report(&counts, &ids);
    if report != want {
        run.fail(&format!(
            "expected the import report {want:?}, got {report:?}"
        ));
        return run.finish();
    }

    // Text cannot grant trust, even when the export says it should, and it
    // never arrives as though atomic wrote it.
    let Some(items) = run.ok("all_items", || store.all_items(atom())).await else {
        return run.finish();
    };
    for item in &items {
        if item.trust != Trust::External {
            run.fail(&format!(
                "{} came in as {} and must be external",
                item.id, item.trust
            ));
        }
        if item.created_by != "bd" {
            run.fail(&format!(
                "{} says {} wrote it, and an import is not atomic writing it",
                item.id, item.created_by
            ));
        }
    }

    let Some(ready_items) = run.ok("ready", || store.ready(atom(), 100)).await else {
        return run.finish();
    };
    let mut ready: Vec<String> = ready_items.into_iter().map(|item| item.id).collect();
    ready.sort();
    if ready != want_ready {
        run.fail(&format!("expected ready {want_ready:?}, got {ready:?}"));
    }

    // The body keeps its shape; the blocked item knows its blocker and its own
    // status.
    let epic = format!("{at}-syn-epic");
    let shown = run
        .some(&format!("{epic} after the import"), || {
            store.show(atom(), epic.clone())
        })
        .await;
    let Some(shown) = shown else {
        return run.finish();
    };
    if !shown.item.body.contains("Synthetic design.") {
        run.fail(&format!(
            "{epic} lost its body sections: {:?}",
            shown.item.body
        ));
    }
    if shown.item.parent != Some(epic.clone()) && shown.item.title != "Synthetic epic" {
        run.fail(&format!("{epic} is not the epic the file described"));
    }
    let blocked = format!("{at}-syn-e");
    let shown = run
        .some(&format!("{blocked} after the import"), || {
            store.show(atom(), blocked.clone())
        })
        .await;
    let Some(shown) = shown else {
        return run.finish();
    };
    if shown.blocked_by != [format!("{at}-syn-c")] {
        run.fail(&format!(
            "{blocked} should be held by the in-progress item, got {:?}",
            shown.blocked_by
        ));
    }
    if shown.item.status != Status::Blocked {
        run.fail(&format!(
            "{blocked} came in {} and should stay blocked",
            shown.item.status
        ));
    }
    let child = format!("{at}-syn-a");
    let shown = run
        .some(&format!("{child} after the import"), || {
            store.show(atom(), child.clone())
        })
        .await;
    let Some(shown) = shown else {
        return run.finish();
    };
    if shown.item.parent != Some(epic) {
        run.fail(&format!(
            "{child} should belong to the epic, got {:?}",
            shown.item.parent
        ));
    }

    // Notes from the file and comments from their own authors, oldest first,
    // all of it external.
    let with_comments = format!("{at}-syn-f");
    let shown = run
        .some(&format!("{with_comments} after the import"), || {
            store.show(atom(), with_comments.clone())
        })
        .await;
    let Some(shown) = shown else {
        return run.finish();
    };
    let authors: Vec<(String, String)> = shown
        .notes
        .iter()
        .map(|note| (note.author.clone(), note.text.clone()))
        .collect();
    let want = vec![
        (
            "bd:Synthetic Person".to_string(),
            "A synthetic comment.".to_string(),
        ),
        (
            "bd:Synthetic Other".to_string(),
            "Another synthetic comment.".to_string(),
        ),
    ];
    if authors != want {
        run.fail(&format!(
            "expected the file's comments in order with their authors, got {authors:?}"
        ));
    }
    for note in &shown.notes {
        if note.trust != Trust::External {
            run.fail(&format!(
                "{} is imported text and reads {}",
                with_comments, note.trust
            ));
        }
    }

    // An imported item works like any other: the one bd had in progress can be
    // claimed by name, at the first epoch.
    let in_progress = format!("{at}-syn-c");
    let claimed = run
        .some("an imported in-progress item to be claimed", || {
            store.claim(
                atom(),
                "taker".to_string(),
                in_progress.clone(),
                Duration::from_secs(60),
            )
        })
        .await;
    if let Some(lease) = claimed
        && lease.epoch != 1
    {
        run.fail(&format!(
            "the first claim of {in_progress} is at epoch {}",
            lease.epoch
        ));
    }

    // A token in the file is not kept, in an item's text or in any note.
    let found = items
        .iter()
        .filter(|item| item.title.contains("ghp_") || item.body.contains("ghp_"))
        .map(|item| item.id.clone())
        .collect::<Vec<String>>();
    if !found.is_empty() {
        run.fail(&format!(
            "these items kept a token from the file: {found:?}"
        ));
    }
    for id in items.iter().map(|item| item.id.clone()) {
        let shown = run
            .some(&format!("{id} to look for the token"), || {
                store.show(atom(), id.clone())
            })
            .await;
        if let Some(shown) = shown
            && shown.notes.iter().any(|note| note.text.contains("ghp_"))
        {
            run.fail(&format!("{id} kept a token in a note"));
        }
    }
    run.finish()
}

/// Port of `importing_again_changes_nothing`, which is M5's "a second import of
/// one beads export is a no-op".
pub async fn importing_a_second_time_changes_nothing<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    if run
        .ok("the first import", || {
            store.import(atom(), input.clone(), false)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    let Some(before) = snapshot(&mut run, store).await else {
        return run.finish();
    };
    let Some(again) = run
        .ok("the second import", || {
            store.import(atom(), input.clone(), false)
        })
        .await
    else {
        return run.finish();
    };
    if !again.added.is_empty() {
        run.fail(&format!("the second import added {:?}", again.added));
    }
    if !again.left_as_is.is_empty() {
        run.fail(&format!(
            "the second import left things as is: {:?}",
            again.left_as_is
        ));
    }
    if again.unchanged.len() != again.items {
        run.fail(&format!(
            "expected {} unchanged, got {}",
            again.items,
            again.unchanged.len()
        ));
    }
    if (
        again.dependencies_added,
        again.parents_set,
        again.notes_added,
    ) != (0, 0, 0)
    {
        run.fail(&format!(
            "the second import wrote again: {} edges, {} parents, {} notes",
            again.dependencies_added, again.parents_set, again.notes_added
        ));
    }
    let Some(after_import) = snapshot(&mut run, store).await else {
        return run.finish();
    };
    if after_import != before {
        run.fail("a second import of one export changed the store");
        return run.finish();
    }

    // Work done in atomic since the import stays, and the same export is still
    // counted as imported.
    let epic = format!("{at}-syn-epic");
    let claimed = run
        .some("the imported epic to be claimed", || {
            store.claim(atom(), "worker".to_string(), epic, Duration::from_secs(60))
        })
        .await;
    if claimed.is_none() {
        return run.finish();
    }
    let Some(after_work) = snapshot(&mut run, store).await else {
        return run.finish();
    };
    if after_work == before {
        run.fail("claiming the imported epic changed nothing in the store");
    }
    let Some(third) = run
        .ok("the third import", || store.import(atom(), input, false))
        .await
    else {
        return run.finish();
    };
    if third.unchanged.len() != third.items {
        run.fail(&format!(
            "the same export after atomic's own work changed {} things",
            third.items - third.unchanged.len()
        ));
    }
    let Some(after_third) = snapshot(&mut run, store).await else {
        return run.finish();
    };
    if after_third != after_work {
        run.fail("the export overwrote work atomic had done since");
    }
    run.finish()
}

/// Port of `a_changed_export_adds_only_what_is_new_and_leaves_what_differs`.
pub async fn a_changed_export_adds_only_what_is_new<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    if run
        .ok("the first import", || {
            store.import(atom(), input.clone(), false)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    let changed = input.replace("blocked by one in progress", "renamed in bd");
    let changed = format!(
        "{}{{\"id\":\"{at}-syn-g\",\"title\":\"Synthetic new\",\"status\":\"open\",\"priority\":3,\"issue_type\":\"task\",\"created_at\":\"2026-01-09T00:00:00Z\",\"dependencies\":[{{\"issue_id\":\"{at}-syn-g\",\"depends_on_id\":\"{at}-syn-a\",\"type\":\"blocks\"}}]}}\n",
        changed
    );
    let Some(report) = run
        .ok("the changed import", || {
            store.import(atom(), changed, false)
        })
        .await
    else {
        return run.finish();
    };
    if report.added != [format!("{at}-syn-g")] {
        run.fail(&format!(
            "expected only the new item added, got {:?}",
            report.added
        ));
    }
    let want = vec![(
        format!("{at}-syn-a"),
        LeftAlone::ImportedBeforeFromDifferentText,
    )];
    if report.left_as_is != want {
        run.fail(&format!(
            "expected the renamed item left as is for its own reason, got {:?}",
            report.left_as_is
        ));
    }
    if report.unchanged.len() != 6 {
        run.fail(&format!(
            "expected six items unchanged, got {}",
            report.unchanged.len()
        ));
    }
    if report.dependencies_added != 1 {
        run.fail(&format!(
            "expected the one new edge added, got {}",
            report.dependencies_added
        ));
    }
    let kept = run
        .some("the item bd renamed", || {
            store.show(atom(), format!("{at}-syn-a"))
        })
        .await;
    if let Some(kept) = kept
        && !kept.item.title.contains("blocked by one in progress")
    {
        run.fail(&format!(
            "the import overwrote a title atomic already had: {:?}",
            kept.item.title
        ));
    }
    let new = run
        .some("the new item", || store.show(atom(), format!("{at}-syn-g")))
        .await;
    if let Some(new) = new
        && new.blocked_by != [format!("{at}-syn-a")]
    {
        run.fail(&format!(
            "the new item should be held by the old one, got {:?}",
            new.blocked_by
        ));
    }
    run.finish()
}

/// Port of `an_item_errai_made_is_never_overwritten`.
pub async fn an_item_atomic_made_is_never_overwritten<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    let mine = format!("{at}-syn-b");
    let draft = Draft::new(&mine, "Made in atomic");
    if run
        .ok("create_item where the file has an id", || {
            store.create_item(atom(), format!("create-{mine}"), draft)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    let Some(report) = run
        .ok("import", || store.import(atom(), input.clone(), false))
        .await
    else {
        return run.finish();
    };
    let want = vec![(mine.clone(), LeftAlone::MadeInAtomic)];
    if report.left_as_is != want {
        run.fail(&format!(
            "expected the item atomic made to be left as is, got {:?}",
            report.left_as_is
        ));
    }
    if report.added.len() != 6 {
        run.fail(&format!(
            "expected the other six items added, got {:?}",
            report.added
        ));
    }
    let shown = run
        .some(&format!("{mine} after the import"), || {
            store.show(atom(), mine.clone())
        })
        .await;
    let Some(shown) = shown else {
        return run.finish();
    };
    if shown.item.title != "Made in atomic" || shown.item.trust != Trust::Agent {
        run.fail(&format!(
            "the import changed an item atomic made: {:?} {}",
            shown.item.title, shown.item.trust
        ));
    }
    // The file's blocker for another item is that id, which exists, so the edge
    // is still added.
    let dependent = format!("{at}-syn-f");
    let shown = run
        .some(&format!("{dependent} after the import"), || {
            store.show(atom(), dependent)
        })
        .await;
    if let Some(shown) = shown
        && !shown.blocked_by.contains(&mine)
    {
        run.fail(&format!(
            "the edge onto the item atomic made was not added: {:?}",
            shown.blocked_by
        ));
    }
    run.finish()
}

/// Port of `a_reference_to_nothing_refuses_the_whole_file`, and of the line in
/// Errai's test where a status no backend has refuses the file before anything
/// is read from the database.
pub async fn an_unusable_line_refuses_the_whole_file<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    for kind in ["blocks", "parent-child"] {
        let dangling = format!(
            "{}{{\"id\":\"{at}-syn-z\",\"title\":\"Synthetic\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-09T00:00:00Z\",\"dependencies\":[{{\"issue_id\":\"{at}-syn-z\",\"depends_on_id\":\"{at}-syn-missing\",\"type\":\"{kind}\"}}]}}\n",
            input
        );
        let refused = run
            .refused(
                &format!("a {kind} edge onto an item the file does not have"),
                |error| {
                    matches!(error, StoreError::ImportRefused(problems)
                        if problems.iter().any(|problem| problem.contains("syn-missing")))
                },
                || store.import(atom(), dangling, false),
            )
            .await;
        if refused.is_none() {
            return run.finish();
        }
        let Some(items) = run
            .ok("all_items after the refusal", || store.all_items(atom()))
            .await
        else {
            return run.finish();
        };
        if !items.is_empty() {
            run.fail(&format!(
                "the refusal of a {kind} edge onto nothing still wrote {} items",
                items.len()
            ));
            return run.finish();
        }
    }

    // A status no backend has refuses the file too, and says which line.
    let unknown = input.replace(r#""status":"deferred""#, r#""status":"teleported""#);
    let refused = run
        .refused(
            "a status no backend has",
            |error| {
                matches!(error, StoreError::ImportRefused(problems)
                    if problems.iter().any(|problem| problem.contains("teleported")))
            },
            || store.import(atom(), unknown, false),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    let Some(items) = run
        .ok("all_items after the unknown status", || {
            store.all_items(atom())
        })
        .await
    else {
        return run.finish();
    };
    if !items.is_empty() {
        run.fail(&format!(
            "an unknown status still wrote {} items",
            items.len()
        ));
    }
    run.finish()
}

/// The other half of the rule Errai reached only by tampering: beads marks a
/// pinned item with a flag, so a real export does not carry `pinned` as a
/// status. An importer that accepted it would quietly turn a thread root into
/// ordinary work, so atomic refuses it and says what it cannot write.
pub async fn a_status_atomic_cannot_write_refuses_the_file<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    let pinned = input.replace(r#""status":"open""#, r#""status":"pinned""#);
    let refused = run
        .refused(
            "a pinned status, which atomic keeps as a flag",
            |error| {
                matches!(error, StoreError::ImportRefused(problems)
                    if problems.iter().any(|problem| problem.contains("pinned")))
            },
            || store.import(atom(), pinned, false),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    let Some(items) = run
        .ok("all_items after the refusal", || store.all_items(atom()))
        .await
    else {
        return run.finish();
    };
    if !items.is_empty() {
        run.fail(&format!(
            "the refusal of a pinned status still wrote {} items",
            items.len()
        ));
    }
    run.finish()
}

/// Port of `a_dry_run_writes_nothing`.
pub async fn a_dry_run_writes_nothing<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    let (counts, _) = expected(&input);
    let refused = run
        .refused(
            "a dry run",
            |error| {
                matches!(error, StoreError::ImportDryRun(report)
                    if report.added.len() == counts.items)
            },
            || store.import(atom(), input, true),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    let Some(items) = run
        .ok("all_items after the dry run", || store.all_items(atom()))
        .await
    else {
        return run.finish();
    };
    if !items.is_empty() {
        let ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
        run.fail(&format!("a dry run wrote {} items: {ids:?}", items.len()));
    }
    run.finish()
}

/// Port of `the_synthetic_counts_are_counted_the_same_way`: the fixture's own
/// counts, taken from its text, against what the store holds.
pub async fn the_synthetic_counts_are_counted_the_same_way<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let input = reroute(SYNTHETIC, at);
    let (counts, _) = expected(&input);
    // The fixture and this counter are read together, so a fixture that stops
    // counting out as these checks assume fails here first, with the counts.
    // Three `blocks` edges, one parent, four notes (two `notes` fields and two
    // comments), and two links that must not block.
    let want = Counts {
        items: 7,
        blocking: 3,
        parents: 1,
        notes: 4,
        relations: 2,
    };
    if counts != want {
        run.fail(&format!(
            "the fixture no longer counts out as these checks assume: wanted {want:?}, got {counts:?}"
        ));
        return run.finish();
    }
    if run
        .ok("import", || store.import(atom(), input.clone(), false))
        .await
        .is_none()
    {
        return run.finish();
    }
    let Some(items) = run.ok("all_items", || store.all_items(atom())).await else {
        return run.finish();
    };
    if items.len() != counts.items {
        run.fail(&format!(
            "the file has {} items and the store holds {}",
            counts.items,
            items.len()
        ));
    }
    let mut blocking = 0;
    let mut parents = 0;
    let mut notes = 0;
    for item in &items {
        parents += usize::from(item.parent.is_some());
        let shown = run
            .some(&format!("{} for its edges", item.id), || {
                store.show(atom(), item.id.clone())
            })
            .await;
        let Some(shown) = shown else {
            return run.finish();
        };
        blocking += shown.blocked_by.len();
        notes += shown.notes.len();
    }
    // `show().blocked_by` is the active projection, not a count of all
    // imported edges. The fixture has one `blocks` edge to a closed target;
    // its imported open epic is a hierarchy container, not an active blocker.
    let lines: Vec<Line> = input
        .lines()
        .filter(|line| !line.is_empty())
        .map(read)
        .collect();
    let statuses: BTreeMap<&str, &str> = lines
        .iter()
        .map(|line| (line.id.as_str(), line.status.as_str()))
        .collect();
    let active_blocks = lines
        .iter()
        .flat_map(|line| &line.blockers)
        .filter(|target| statuses.get(target.as_str()) != Some(&"closed"))
        .count();
    if blocking != active_blocks {
        run.fail(&format!(
            "the file has {active_blocks} active blocking edges and the store has {blocking}"
        ));
    }
    if parents != counts.parents {
        run.fail(&format!(
            "the file has {} parents and the store has {parents}",
            counts.parents
        ));
    }
    if notes != counts.notes {
        run.fail(&format!(
            "the file has {} notes and the store has {notes}",
            counts.notes
        ));
    }
    run.finish()
}

/// The ids the file names, in the order it names them.
fn want_ids(input: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        ids.push(
            value(line, "id")
                .unwrap_or_else(|| panic!("the fixture has a line with no id: {line}")),
        );
    }
    ids
}

/// Give the fixture's ids a prefix of their own, so one store can hold every
/// scenario in the suite. Everything a check counts is counted from the
/// rewritten text, so the counter and the store read the same file.
fn reroute(input: &str, at: &str) -> String {
    input.replace("syn-", &format!("{at}-syn-"))
}
