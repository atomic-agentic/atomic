// SPDX-License-Identifier: Apache-2.0

//! Close policies, ported from Errai's `errai-tracker` close-policy tests.
//!
//! Ported on 2026-11-23 from `crates/errai-tracker/tests/close_policy.rs` in the
//! owner-approved Errai reference checkout. Errai set the policy by writing to
//! its `close_policy` table as its `errai_owner` role and checked refusals
//! against Postgres constraints; atomic has one store API, so the same rules are
//! asserted through it:
//!
//! | Errai | Here |
//! | --- | --- |
//! | `INSERT INTO close_policy …` as `errai_owner` | `set_close_policy` under a person's credential |
//! | the app role's `permission denied` on that table | `StoreError::NeedsHumanCredential`, refused by the store rather than by a grant |
//! | `INSERT INTO link … 'session'` / `'item'` | `link_commit`, which names the item through the lease it was made under |
//! | `INSERT INTO repo_commit … on_branch` | the `indexed` and `on_branch` facts `link_commit` carries, which come from atomic's index and not from the atom |
//! | `close_policy_requires_check` refusing `signed_commit` | refused by the closed [`Evidence`] set: a requirement atomic cannot enforce has no name to be written |
//! | another tenant's policy not applying | deferred with tenancy, as in the lease checks (M7) |
//!
//! What the policy is for is unchanged: an item that cannot show what it asks
//! for stays held, and stays releasable, rather than closing with a claim in it.

use crate::harness::Clock;
use crate::harness::LONG;
use crate::harness::Run;
use crate::harness::atom;
use crate::harness::person;
use crate::store_contract::Draft;
use crate::store_contract::Evidence;
use crate::store_contract::Finish;
use crate::store_contract::GraphStore;
use crate::store_contract::Lease;
use crate::store_contract::Status;
use crate::store_contract::StoreError;

/// The commit every policy check links, of the shape atomic's worktree step
/// reports: forty hex digits.
const SHA: &str = "c000000000000000000000000000000000000000";

/// Create `id` and hold it, which is the state a close is asked for in.
async fn held<S: GraphStore>(run: &mut Run, store: &S, id: &str) -> Option<Lease> {
    let draft = Draft::new(id, "synthetic");
    run.ok("create_item", || {
        store.create_item(atom(), format!("create-{id}"), draft)
    })
    .await?;
    run.some(&format!("a claim of {id}"), || {
        store.claim(atom(), "worker".to_string(), id.to_string(), LONG)
    })
    .await
}

/// The policy a person sets, which is the only way one gets set.
async fn policy<S: GraphStore>(run: &mut Run, store: &S, requires: &[Evidence]) -> bool {
    run.ok("set_close_policy as a person", || {
        store.set_close_policy(person(), requires.to_vec())
    })
    .await
    .is_some()
}

/// A verified policy names the item and branch after the item exists.
async fn verified_item_policy<S: GraphStore>(
    run: &mut Run,
    store: &S,
    id: &str,
    requires: &[Evidence],
) -> bool {
    run.ok("set_item_close_policy as a person", || {
        store.set_item_close_policy(
            person(),
            id.to_owned(),
            requires.to_vec(),
            Some("refs/heads/main".to_owned()),
        )
    })
    .await
    .is_some()
}

/// Where the item is now.
async fn status<S: GraphStore>(run: &mut Run, store: &S, id: &str) -> Option<Status> {
    Some(
        run.some(&format!("{id} to read its status"), || {
            store.show(atom(), id.to_string())
        })
        .await?
        .item
        .status,
    )
}

/// A refusal to close for a named piece of evidence.
fn needs(item: &str, required: &str) -> impl Fn(&StoreError) -> bool {
    let item = item.to_string();
    let required = required.to_string();
    move |error| {
        matches!(error, StoreError::NeedsEvidence { item_id, required: want }
            if *item_id == item && *want == required)
    }
}

/// Port of `with_no_policy_an_item_closes_without_a_commit`, including that an
/// empty list is the same as no policy at all.
pub async fn with_no_policy_an_item_closes_without_a_commit<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if run
        .ok("close with no policy", || {
            store.finish(lease, Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::Closed) {
        return run.finish();
    }

    let first = format!("{at}-empty");
    if !policy(&mut run, store, &[]).await {
        return run.finish();
    }
    let Some(lease) = held(&mut run, store, &first).await else {
        return run.finish();
    };
    if run
        .ok("close under an empty policy", || {
            store.finish(lease, Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, &first).await != Some(Status::Closed) {
        run.fail(&format!(
            "{first} should close under an empty policy, as it does under none"
        ));
    }
    run.finish()
}

/// Port of `under_a_commit_policy_an_item_without_one_stays_held`: the close is
/// refused, the item stays held, the lease still works, and the holder can still
/// give the item back unfinished.
pub async fn under_a_commit_policy_an_item_without_one_stays_held<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if run
        .refused("a close with no commit", needs(at, "commit"), || {
            store.finish(lease.clone(), Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::InProgress) {
        return run.finish();
    }
    // The lease still works, so the holder can still give the item back.
    if run
        .ok("release under the policy", || {
            store.finish(lease, Finish::Release)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::Open) {
        run.fail(&format!("{at} was given back and is not in the queue"));
    }
    run.finish()
}

/// Port of `a_commit_linked_to_the_item_lets_it_close`, and of the retry that
/// follows it: a second close under the same lease succeeds and changes nothing.
pub async fn a_commit_linked_to_the_item_lets_it_close<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if run
        .ok("link_commit", || {
            store.link_commit(lease.clone(), SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if run
        .ok("close with its own commit", || {
            store.finish(lease.clone(), Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::Closed) {
        return run.finish();
    }
    if run
        .ok("close again under the same lease", || {
            store.finish(lease, Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::Closed) {
        run.fail(&format!("{at} was closed and the retry changed it"));
    }
    run.finish()
}

/// The half of Errai's test that said *the session's own commit is not the
/// item's*: a commit linked under another item's lease is no evidence here.
pub async fn another_items_commit_is_not_this_items<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let other = format!("{at}-other");
    let Some(wrong) = held(&mut run, store, &other).await else {
        return run.finish();
    };
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if run
        .ok("link the commit to the other item", || {
            store.link_commit(wrong.clone(), SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if run
        .refused(
            "a close on another item's commit",
            needs(at, "commit"),
            || store.finish(lease.clone(), Finish::Close),
        )
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::InProgress) {
        return run.finish();
    }
    // And with the same commit under its own lease, it closes.
    if run
        .ok("link the commit to the item", || {
            store.link_commit(lease.clone(), SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if run
        .ok("close with its own commit", || {
            store.finish(lease, Finish::Close)
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::Closed) {
        run.fail(&format!("{at} had its own commit and did not close"));
    }
    run.finish()
}

/// Port of `the_server_role_can_only_read_the_policy`. Errai refused the model's
/// database role with `permission denied`; atomic has no role per connection, so
/// the store refuses the action for an actor without a human credential. That is
/// the repo rule — an admin action comes only from a human credential, never from
/// a model's tool call or an atom's shell — checked where atomic can enforce it.
pub async fn the_policy_is_a_persons_to_set<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let refused = run
        .refused(
            "an agent loosening the policy",
            |error| {
                matches!(error, StoreError::NeedsHumanCredential { action }
                    if action.contains("close policy"))
            },
            || store.set_close_policy(atom(), Vec::new()),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    // The policy the person set still holds, so the refused write changed
    // nothing.
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    run.refused(
        "a close with no commit, after the refused write",
        needs(at, "commit"),
        || store.finish(lease, Finish::Close),
    )
    .await;
    run.finish()
}

/// The model-facing link cannot assert Git facts. A verified policy refuses
/// both a missing link and repeated caller-provided SHAs; the concrete host
/// proof path has its own real-Git test.
pub async fn a_verified_commit_must_be_indexed_and_reached<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if !verified_item_policy(&mut run, store, at, &[Evidence::VerifiedCommit]).await {
        return run.finish();
    }
    if run
        .refused(
            "a close with no commit at all",
            needs(at, "verified_commit"),
            || store.finish(lease.clone(), Finish::Close),
        )
        .await
        .is_none()
    {
        return run.finish();
    }
    if run
        .ok("link the caller's unverified commit", || {
            store.link_commit(lease.clone(), SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    if run
        .refused(
            "caller link cannot mint verification",
            needs(at, "verified_commit"),
            || store.finish(lease.clone(), Finish::Close),
        )
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::InProgress) {
        run.fail(&format!("{at} did not stay held after unverified link"));
    }
    run.finish()
}

/// Port of `a_verified_commit_must_be_the_items_own`, with the two requirements
/// as their conjunction. Errai's other half — a policy naming `signed_commit`
/// refused by a database check — has no twin here: [`Evidence`] is a closed set,
/// so a requirement atomic cannot enforce cannot be written at all, which is the
/// same rule enforced before the value is stored rather than in a constraint.
pub async fn both_requirements_need_their_own_pieces<S: GraphStore>(
    store: &S,
    _clock: &Clock,
    at: &str,
) -> Vec<String> {
    let mut run = Run::new();
    if !policy(&mut run, store, &[Evidence::Commit]).await {
        return run.finish();
    }
    let other = format!("{at}-other");
    let Some(wrong) = held(&mut run, store, &other).await else {
        return run.finish();
    };
    let Some(lease) = held(&mut run, store, at).await else {
        return run.finish();
    };
    if !verified_item_policy(
        &mut run,
        store,
        at,
        &[Evidence::Commit, Evidence::VerifiedCommit],
    )
    .await
    {
        return run.finish();
    }
    // A commit that belongs to another item is not this item's
    // evidence: the plain commit requirement is the one that refuses first.
    if run
        .ok("link a commit to the other item", || {
            store.link_commit(wrong, SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    let refused = run
        .refused(
            "a close on another item's verified commit",
            |error| matches!(error, StoreError::NeedsEvidence { item_id, .. } if *item_id == at),
            || store.finish(lease.clone(), Finish::Close),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    // This item's commit, not indexed: still held, for the other requirement.
    if run
        .ok("link this item's commit, unverified", || {
            store.link_commit(lease.clone(), SHA.to_string())
        })
        .await
        .is_none()
    {
        return run.finish();
    }
    let refused = run
        .refused(
            "a close with an unverified commit of its own",
            |error| {
                matches!(error, StoreError::NeedsEvidence { item_id, required } if *item_id == at && required == "verified_commit")
            },
            || store.finish(lease.clone(), Finish::Close),
        )
        .await;
    if refused.is_none() {
        return run.finish();
    }
    if run
        .refused(
            "caller link still cannot meet verified evidence",
            needs(at, "verified_commit"),
            || store.finish(lease.clone(), Finish::Close),
        )
        .await
        .is_none()
    {
        return run.finish();
    }
    if status(&mut run, store, at).await != Some(Status::InProgress) {
        run.fail(&format!("{at} did not stay held without host verification"));
    }
    run.finish()
}
