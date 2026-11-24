// SPDX-License-Identifier: Apache-2.0

//! Import and export the project's work graph through the public CLI.

use anyhow::{Context, bail};
use atomic_app::pipeline::{GRAPH_PIPELINE, GraphPipelineRun, LAYOUT_VERSION, Step};
use atomic_core::git::Git;
use atomic_core::state::ProjectState;
use atomic_core::workflow::{Criterion, parse_criteria};
use atomic_core::worktree::Worktrees;
use atomic_durable::publication::{Acceptance, Candidate};
use atomic_durable::{Database, DurableRuntime, Durare};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{
    Actor, ClosePolicySource, Draft, EdgeKind, Evidence, GraphStore, ItemClosePolicy, Report,
    Status, StoreError,
};
use atomic_graph::transactional_claim::ClaimNext;
use clap::{Subcommand, ValueEnum};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::SystemTime;

#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Create a committed work item as the enrolled operator.
    Create {
        /// One-line title.
        title: String,
        /// Explicit item ID; otherwise a fresh short hash is used.
        #[arg(long)]
        id: Option<String>,
        /// Body or acceptance criteria.
        #[arg(long, default_value = "")]
        body: String,
        /// Item kind, such as task or bug.
        #[arg(long, default_value = "task")]
        kind: String,
        /// Lower numbers come first in ready results.
        #[arg(long, default_value_t = 2)]
        priority: i32,
        /// Create a pinned epic whose children can be worked by `atomic run --graph-epic`.
        #[arg(long)]
        pinned: bool,
    },
    /// Add a committed edge between existing items.
    Link {
        from: String,
        kind: String,
        to: String,
    },
    /// Claim an item as the enrolled operator.
    Claim { id: String },
    /// Pin an open epic as the enrolled operator so a graph run can work its children.
    Pin { id: String },
    /// Release a failed graph run's epic binding after checking its terminal state.
    ReconcileFailed {
        /// The pinned epic bound to the failed run.
        epic: String,
        /// Exact failed run ID; a different active run is never displaced.
        #[arg(long)]
        run: String,
    },
    /// Close an item as the enrolled operator after checking its evidence policy.
    Close { id: String },
    /// Verify a recorded worktree attempt against an item's authorized branch.
    VerifyCommit {
        id: String,
        /// The graph run that owns the item's current claim.
        #[arg(long)]
        run: String,
        /// The recorded graph step, such as graph.00000.fix.
        #[arg(long)]
        step: String,
        /// The worktree attempt number, starting at 1.
        #[arg(long)]
        attempt: u32,
    },
    /// Set, inspect or clear a named item's close evidence requirements.
    Policy {
        #[command(subcommand)]
        action: GraphPolicyCommand,
    },
    /// Set or inspect human-approved executable criteria on one graph item.
    Criteria {
        #[command(subcommand)]
        action: GraphCriteriaCommand,
    },
    /// Import a beads JSONL export into this project's graph.
    Import {
        /// JSONL file, or `-` to read standard input.
        file: PathBuf,
        /// Show what would be imported without changing the graph.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write this project's graph in beads JSONL format to standard output.
    Export,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum PolicyRequirement {
    Commit,
    VerifiedCommit,
}

impl From<PolicyRequirement> for Evidence {
    fn from(value: PolicyRequirement) -> Self {
        match value {
            PolicyRequirement::Commit => Evidence::Commit,
            PolicyRequirement::VerifiedCommit => Evidence::VerifiedCommit,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum GraphPolicyCommand {
    /// Require evidence before this item can be closed.
    Set {
        id: String,
        /// `commit` or `verified-commit`; repeat to require both.
        #[arg(long = "require", value_enum, required = true)]
        requires: Vec<PolicyRequirement>,
        /// Exact authorized branch for verified-commit, e.g. refs/heads/main.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Show the effective item rule or the inherited legacy rule.
    Show { id: String },
    /// Remove this item's rule, restoring any legacy default.
    Clear { id: String },
}

#[derive(Debug, Subcommand)]
pub enum GraphCriteriaCommand {
    /// Replace one item's structured criteria with a JSON list.
    Set {
        id: String,
        /// JSON list file, or `-` to read standard input.
        #[arg(long)]
        file: PathBuf,
    },
    /// Show the structured criteria approved for one item.
    Show { id: String },
    /// Remove the structured criteria from one item.
    Clear { id: String },
}

fn policy_json(id: &str, policy: Option<&ItemClosePolicy>) -> serde_json::Value {
    json!({
        "item_id": id,
        "policy": policy.map(|policy| json!({
            "requires": policy.requires.iter().map(|requirement| requirement.as_str()).collect::<Vec<_>>(),
            "authorized_branch": policy.authorized_branch,
            "source": match policy.source {
                ClosePolicySource::Item => "item",
                ClosePolicySource::LegacyGlobal => "legacy_global",
            },
            "set_by": policy.set_by,
        })),
    })
}

fn print_policy(id: &str, policy: Option<&ItemClosePolicy>, as_json: bool) -> anyhow::Result<()> {
    if as_json {
        println!("{}", serde_json::to_string(&policy_json(id, policy))?);
    } else if let Some(policy) = policy {
        let requirements = policy
            .requires
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let source = match policy.source {
            ClosePolicySource::Item => "item rule",
            ClosePolicySource::LegacyGlobal => "legacy rule",
        };
        match policy.authorized_branch.as_deref() {
            Some(branch) => println!("{id}: {requirements} on {branch} ({source})"),
            None => println!("{id}: {requirements} ({source})"),
        }
    } else {
        println!("{id}: no close policy");
    }
    Ok(())
}

fn policy_write(action: &GraphPolicyCommand) -> bool {
    matches!(
        action,
        GraphPolicyCommand::Set { .. } | GraphPolicyCommand::Clear { .. }
    )
}

fn criteria_write(action: &GraphCriteriaCommand) -> bool {
    matches!(
        action,
        GraphCriteriaCommand::Set { .. } | GraphCriteriaCommand::Clear { .. }
    )
}

fn read_criteria(path: &Path) -> anyhow::Result<Vec<Criterion>> {
    let mut source = String::new();
    if path == Path::new("-") {
        std::io::stdin()
            .read_to_string(&mut source)
            .context("read criteria JSON from standard input")?;
    } else {
        source = std::fs::read_to_string(path)
            .with_context(|| format!("read criteria JSON from {}", path.display()))?;
    }
    let value: serde_json::Value = serde_json::from_str(&source)
        .with_context(|| format!("parse criteria JSON from {}", path.display()))?;
    parse_criteria(&value).map_err(|error| anyhow::anyhow!("invalid criteria: {error}"))
}

fn print_criteria(
    id: &str,
    record: Option<&atomic_graph::store::ItemCriteria>,
    as_json: bool,
) -> anyhow::Result<()> {
    let value = json!({
        "item_id": id,
        "criteria": record.map_or(&[][..], |record| record.criteria.as_slice()),
        "set_by": record.map(|record| record.set_by.as_str()),
    });
    if as_json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn edge_kind(name: &str) -> anyhow::Result<EdgeKind> {
    use EdgeKind::*;
    let kinds = [
        Blocks,
        ParentChild,
        ConditionalBlocks,
        WaitFor,
        Related,
        DiscoveredFrom,
        RelatesTo,
        Duplicates,
        Supersedes,
        RepliesTo,
    ];
    kinds
        .into_iter()
        .find(|kind| kind.as_str() == name)
        .ok_or_else(|| anyhow::anyhow!("unknown graph edge kind {name:?}"))
}

fn fresh_request() -> anyhow::Result<String> {
    Ok(format!(
        "graph-cli-{}",
        atomic_core::credential::fresh_secret()?
    ))
}

fn graph_claim_step(step: &str, template: &str, repair_rounds: u64) -> Option<(String, String)> {
    let rest = step.strip_prefix("graph.")?;
    let (ordinal, name) = rest.split_once('.')?;
    if ordinal.len() != 5
        || !ordinal.bytes().all(|byte| byte.is_ascii_digit())
        || ordinal.parse::<usize>().ok()? >= 10_000
    {
        return None;
    }
    let base = format!("graph.{ordinal}.{template}");
    if name != template {
        let round = name.strip_prefix(template)?.strip_prefix(".repair.")?;
        let parsed = round.parse::<u64>().ok()?;
        if parsed == 0 || parsed > repair_rounds || parsed.to_string() != round {
            return None;
        }
    }
    Some((format!("graph.{ordinal}.claim"), base))
}

fn read_import(path: &Path) -> anyhow::Result<String> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::stdin()
            .read_to_string(&mut text)
            .context("read beads JSONL from standard input")?;
    } else {
        text = std::fs::read_to_string(path)
            .with_context(|| format!("read beads JSONL from {}", path.display()))?;
    }
    Ok(text)
}

fn print_report(report: &Report, dry_run: bool, as_json: bool) -> anyhow::Result<()> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "dry_run": dry_run,
                "items": report.items,
                "added": report.added,
                "unchanged": report.unchanged,
                "left_as_is": report.left_as_is.iter().map(|(id, why)| {
                    json!({"id": id, "reason": why.to_string()})
                }).collect::<Vec<_>>(),
                "dependencies_added": report.dependencies_added,
                "parents_set": report.parents_set,
                "notes_added": report.notes_added,
                "in_progress": report.in_progress,
                "external": report.external,
                "kept": report.kept,
            }))?
        );
    } else {
        let prefix = if dry_run { "dry run: " } else { "" };
        println!(
            "{prefix}{} item(s): {} added, {} unchanged, {} left as is",
            report.items,
            report.added.len(),
            report.unchanged.len(),
            report.left_as_is.len()
        );
    }
    Ok(())
}

pub async fn run(command: GraphCommand, state: &ProjectState, as_json: bool) -> anyhow::Result<()> {
    let database = crate::engine::database(state)?;
    if matches!(database, Database::Postgres { .. }) {
        bail!("work-graph commands currently require a SQLite project store");
    }
    let path = state.database();
    // Keep the CLI's normal refusal of a store written by a newer atomic.
    // This runs only after the configured database was shown to be SQLite.
    atomic_durable::store::open_sqlite(&path).await?;
    let privileged = match &command {
        GraphCommand::Policy { action } => policy_write(action),
        GraphCommand::Criteria { action } => criteria_write(action),
        _ => matches!(
            &command,
            GraphCommand::Create { .. }
                | GraphCommand::Link { .. }
                | GraphCommand::Claim { .. }
                | GraphCommand::Pin { .. }
                | GraphCommand::ReconcileFailed { .. }
                | GraphCommand::Close { .. }
                | GraphCommand::VerifyCommit { .. }
                | GraphCommand::Import { dry_run: false, .. }
        ),
    };
    let store = if privileged {
        SqliteStore::open_authorized(
            &path,
            SystemTime::now,
            crate::gates::operator_credential(state)?,
        )
    } else {
        SqliteStore::open(&path, SystemTime::now)
    }
    .with_context(|| format!("open the work graph at {}", path.display()))?;
    let actor = if privileged {
        Actor::human("operator")
    } else {
        Actor::agent("atomic graph CLI")
    };
    match command {
        GraphCommand::Create {
            title,
            id,
            body,
            kind,
            priority,
            pinned,
        } => {
            let request = fresh_request()?;
            let id = id
                .unwrap_or_else(|| atomic_graph::sqlite::hash_id("graph-cli", request.as_bytes()));
            let mut draft = Draft::new(&id, &title);
            draft.body = body;
            draft.kind = kind;
            draft.priority = priority;
            if pinned {
                if draft.kind != "epic" {
                    bail!("--pinned requires --kind epic");
                }
                draft.status = Status::Pinned;
            }
            let item = store.create_item(actor, request, draft).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({"id":item.id,"title":item.title,
                    "status":item.status.as_str(),"trust":item.trust.as_str(),"created_by":item.created_by}))?
                );
            } else {
                println!("{}", item.id);
            }
            Ok(())
        }
        GraphCommand::Link { from, kind, to } => {
            let kind = edge_kind(&kind)?;
            store
                .add_edge(actor, from.clone(), kind, to.clone())
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({"from":from,"kind":kind.as_str(),"to":to}))?
                );
            } else {
                println!("linked {from} {} {to}", kind.as_str());
            }
            Ok(())
        }
        GraphCommand::Claim { id } => {
            let lease = store
                .claim(actor, String::new(), id.clone(), Duration::from_secs(900))
                .await?
                .ok_or_else(|| anyhow::anyhow!("item {id} is unavailable to claim"))?;
            if as_json {
                println!("{}", serde_json::to_string(&lease)?);
            } else {
                println!("claimed {} at epoch {}", lease.item_id, lease.epoch);
            }
            Ok(())
        }
        GraphCommand::Pin { id } => {
            store.set_status(actor, id.clone(), Status::Pinned).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({"id":id,"status":"pinned"}))?
                );
            } else {
                println!("pinned {id}");
            }
            Ok(())
        }
        GraphCommand::ReconcileFailed { epic, run } => {
            store.reconcile_failed_epic_run(&epic, &run).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(
                        &json!({"epic":epic,"failed_run":run,"binding":"released"})
                    )?
                );
            } else {
                println!("released failed run {run} from epic {epic}");
            }
            Ok(())
        }
        GraphCommand::Close { id } => {
            store.set_status(actor, id.clone(), Status::Closed).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({"id":id,"status":"closed"}))?
                );
            } else {
                println!("closed {id}");
            }
            Ok(())
        }
        GraphCommand::VerifyCommit {
            id,
            run,
            step,
            attempt,
        } => {
            let rt = Durare::open(
                &database,
                None,
                LAYOUT_VERSION,
                &crate::engine::store_project(state)?,
            )
            .await?;
            let record = rt
                .own_run_record(&run)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no graph run {run} in this project"))?;
            if record.workflow != GRAPH_PIPELINE {
                bail!("run {run} is not a graph epic run");
            }
            let frozen: GraphPipelineRun = serde_json::from_value(record.input)
                .with_context(|| format!("read frozen graph run {run}"))?;
            let template = match frozen.run.pipeline.steps.as_slice() {
                [Step::Atom(template)] if template.worktree => template,
                _ => bail!("graph run {run} has no one-step worktree template"),
            };
            let (claim_step, base_step) = graph_claim_step(
                &step,
                &template.id,
                template.review.as_ref().map_or(1, |review| review.rounds),
            )
            .ok_or_else(|| anyhow::anyhow!("step {step} is not a generated step in run {run}"))?;
            let steps = rt.steps(&run).await?;
            let claim = steps
                .iter()
                .find(|record| record.name == claim_step)
                .and_then(|record| record.output.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("step {claim_step} has no completed claim checkpoint")
                })?;
            let ClaimNext::Claimed { item, lease } = serde_json::from_value(claim)
                .with_context(|| format!("read claim checkpoint {claim_step}"))?
            else {
                bail!("step {claim_step} did not claim an item");
            };
            if item.id != id || lease.item_id != id || lease.claimant != run {
                bail!("step {claim_step} did not claim item {id} for run {run}");
            }
            let release_step = format!("{base_step}.review.claim.release");
            let lease = if let Some(release) =
                steps.iter().find(|record| record.name == release_step)
            {
                let released_epoch: i64 =
                    serde_json::from_value(release.output.clone().ok_or_else(|| {
                        anyhow::anyhow!("step {release_step} has no completed release checkpoint")
                    })?)
                    .with_context(|| format!("read release checkpoint {release_step}"))?;
                let prefix = format!("{base_step}.review.claim.reclaim.");
                let mut reclaimed = Vec::new();
                for record in &steps {
                    let Some(suffix) = record.name.strip_prefix(&prefix) else {
                        continue;
                    };
                    let Ok(occurrence) = suffix.parse::<u64>() else {
                        continue;
                    };
                    if occurrence.to_string() != suffix {
                        continue;
                    }
                    if let Some(value) = record.output.as_ref().filter(|value| !value.is_null()) {
                        let token: atomic_graph::store::Lease =
                            serde_json::from_value(value.clone()).with_context(|| {
                                format!("read re-claim checkpoint {}", record.name)
                            })?;
                        reclaimed.push(token);
                    }
                }
                let [token] = reclaimed.as_slice() else {
                    bail!("run {run} has no unique completed re-claim for {step}");
                };
                if token.item_id != id
                    || token.claimant != run
                    || token.epoch <= released_epoch
                    || released_epoch <= lease.epoch
                {
                    bail!("run {run}'s review re-claim does not match item {id}");
                }
                token.clone()
            } else {
                lease
            };
            let accepted_step = format!("{base_step}.accepted");
            let value = steps
                .iter()
                .find(|record| record.name == accepted_step)
                .and_then(|record| record.output.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("step {accepted_step} has no completed acceptance")
                })?;
            let accepted: Acceptance = serde_json::from_value(value)
                .with_context(|| format!("read acceptance checkpoint {accepted_step}"))?;
            if !accepted.accepted {
                bail!("step {accepted_step} did not accept a candidate");
            }
            let repair_rounds = template.review.as_ref().map_or(1, |review| review.rounds);
            let turn_steps = std::iter::once(format!("{base_step}.turn"))
                .chain((1..=repair_rounds).map(|round| format!("{base_step}.repair.{round}.turn")));
            let mut candidates = Vec::new();
            for turn_step in turn_steps {
                let Some(value) = steps
                    .iter()
                    .find(|record| record.name == turn_step)
                    .and_then(|record| record.output.as_ref())
                    .and_then(|value| value.get("Candidate"))
                else {
                    continue;
                };
                let candidate: Candidate = serde_json::from_value(value.clone())
                    .with_context(|| format!("read candidate checkpoint {turn_step}"))?;
                if accepted.candidate == candidate.id && accepted.digest == candidate.digest {
                    candidates.push(candidate);
                }
            }
            let [candidate] = candidates.as_slice() else {
                bail!(
                    "run {run} does not have exactly one checkpointed accepted candidate for {step}"
                );
            };
            let shown = store
                .show(Actor::agent("atomic graph CLI"), id.clone())
                .await?
                .ok_or_else(|| anyhow::anyhow!("no graph item {id}"))?;
            if shown.item.parent.as_deref() != Some(frozen.epic.as_str()) {
                bail!("item {id} is not a direct child of run {run}'s epic");
            }
            let fixture = frozen.run.pipeline.fixture.ok_or_else(|| {
                anyhow::anyhow!("graph run {run} has no frozen repository fixture")
            })?;
            let repo = Git::find()?.repo(&fixture.snapshot).without_filters()?;
            let worktrees = Worktrees::new(repo, fixture.worktrees);
            if worktrees.base(&run)?.as_deref() != Some(fixture.commit.as_str()) {
                bail!("graph run {run}'s pinned Git base differs from its frozen fixture");
            }
            let step_commit = worktrees
                .recorded_step_commit(&run, &step, attempt)?
                .ok_or_else(|| anyhow::anyhow!("no recorded Git attempt {attempt} for {step}"))?;
            if candidate
                .proposal
                .metadata
                .get("commit")
                .and_then(serde_json::Value::as_str)
                != Some(step_commit.commit.as_str())
                || candidate
                    .proposal
                    .metadata
                    .get("base")
                    .and_then(serde_json::Value::as_str)
                    != Some(step_commit.base.as_str())
                || candidate
                    .proposal
                    .metadata
                    .get("commit_ref")
                    .and_then(serde_json::Value::as_str)
                    != Some(step_commit.reference.as_str())
            {
                bail!("Git attempt {attempt} does not match the step's checkpointed Git record");
            }
            store
                .link_verified_commit(lease, &worktrees, &run, &step_commit)
                .await?;
            let sha = step_commit.commit;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "id": id,
                        "run": run,
                        "step": step,
                        "attempt": attempt,
                        "sha": sha,
                        "verified": true,
                    }))?
                );
            } else {
                println!("verified commit {sha} for {id} on its authorized branch");
            }
            Ok(())
        }
        GraphCommand::Policy { action } => {
            let id = match action {
                GraphPolicyCommand::Set {
                    id,
                    requires,
                    branch,
                } => {
                    let requirements = requires.into_iter().map(Evidence::from).collect();
                    store
                        .set_item_close_policy(actor, id.clone(), requirements, branch)
                        .await?;
                    id
                }
                GraphPolicyCommand::Show { id } => id,
                GraphPolicyCommand::Clear { id } => {
                    store
                        .set_item_close_policy(actor, id.clone(), Vec::new(), None)
                        .await?;
                    id
                }
            };
            let policy = store.item_close_policy(&id).await?;
            print_policy(&id, policy.as_ref(), as_json)
        }
        GraphCommand::Criteria { action } => {
            let id = match action {
                GraphCriteriaCommand::Set { id, file } => {
                    let criteria = read_criteria(&file)?;
                    store.set_item_criteria(actor, id.clone(), criteria).await?;
                    id
                }
                GraphCriteriaCommand::Show { id } => id,
                GraphCriteriaCommand::Clear { id } => {
                    store
                        .set_item_criteria(actor, id.clone(), Vec::new())
                        .await?;
                    id
                }
            };
            let record = store.item_criteria(&id).await?;
            print_criteria(&id, record.as_ref(), as_json)
        }
        GraphCommand::Import { file, dry_run } => {
            let jsonl = read_import(&file)?;
            let report = match store.import(actor, jsonl, dry_run).await {
                Ok(report) => report,
                Err(StoreError::ImportDryRun(report)) if dry_run => *report,
                Err(error) => return Err(error.into()),
            };
            print_report(&report, dry_run, as_json)
        }
        GraphCommand::Export => {
            let jsonl = store.export(actor).await?;
            if as_json {
                println!("{}", serde_json::to_string(&json!({"jsonl": jsonl}))?);
            } else {
                print!("{jsonl}");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::graph_claim_step;

    #[test]
    fn verified_step_names_bind_to_one_generated_claim() {
        assert_eq!(
            graph_claim_step("graph.00042.fix", "fix", 2),
            Some(("graph.00042.claim".into(), "graph.00042.fix".into()))
        );
        assert_eq!(
            graph_claim_step("graph.00042.fix.repair.1", "fix", 2),
            Some(("graph.00042.claim".into(), "graph.00042.fix".into()))
        );
        for wrong in [
            "fix",
            "graph.42.fix",
            "graph.10000.fix",
            "graph.00042.other",
            "graph.00042.fix.turn",
            "graph.00042.fix.repair.0",
            "graph.00042.fix.repair.3",
            "graph.00042.fix.repair.01",
            "graph.00042.fix.repair.1.turn",
            "graph.00042/../fix",
        ] {
            assert!(graph_claim_step(wrong, "fix", 2).is_none(), "{wrong}");
        }
    }
}
