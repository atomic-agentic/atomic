// SPDX-License-Identifier: Apache-2.0

//! Host-only adapter for a delivery provider. The executable and target are
//! installed under the project's protected private directory. One invocation
//! receives one JSON request on stdin and returns one observed state on stdout.
//! No credentials or executable paths come from the workflow or its candidate.

use anyhow::{Context, Result, anyhow, bail, ensure};
use atomic_core::git::{is_ref_name, is_sha};
use atomic_core::state::ProjectState;
use atomic_durable::delivery_effect::{DeliveryPlan, DeliverySlot};
use atomic_exec::process::{self, Limits};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WIRE_LIMIT: usize = 64 * 1024;

/// Operator-maintained `delivery/target.json`. A null expected_old means
/// creation of a previously absent branch. Mutations must use that precondition.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryTarget {
    repository: String,
    branch_ref: String,
    base_ref: String,
    expected_old: Option<String>,
    // If known, bind the existing PR number as well as its repository and refs.
    pull_request_number: Option<u64>,
    title: String,
    body: String,
}

impl DeliveryTarget {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.repository.is_empty()
                && self.repository.len() <= 2048
                && !self.repository.chars().any(char::is_control),
            "invalid delivery repository identity"
        );
        for name in [&self.branch_ref, &self.base_ref] {
            ensure!(
                name.starts_with("refs/heads/") && is_ref_name(name),
                "invalid delivery branch ref"
            );
        }
        ensure!(
            self.branch_ref != self.base_ref,
            "delivery branch equals its base"
        );
        ensure!(
            self.expected_old.as_deref().is_none_or(is_sha),
            "invalid expected old revision"
        );
        ensure!(
            self.pull_request_number != Some(0),
            "invalid pull request number"
        );
        ensure!(
            !self.title.trim().is_empty() && self.title.len() <= 512 && self.body.len() <= 8192,
            "invalid pull request title or body"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DeliveryRemote {
    dir: PathBuf,
    target: DeliveryTarget,
}

/// Reject symlinks and group/other writes to operator-owned inputs. Private
/// state itself is denied to atom shells by ProjectState's sandbox policy.
fn protected(path: &Path, directory: bool) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("read protected delivery adapter installation")?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "delivery installation contains an unexpected file type"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        #[allow(unsafe_code)]
        let owner = unsafe { libc::geteuid() };
        ensure!(
            metadata.uid() == owner && metadata.mode() & 0o022 == 0,
            "delivery installation must be owned by the operator and writable only by that owner"
        );
        if !directory {
            ensure!(
                metadata.nlink() == 1,
                "delivery files must have exactly one hard link"
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Apply,
    Inspect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    key: String,
    state: Option<Observed>,
}

/// A provider reports facts from its own read, including the accepted PR head
/// for merged entries (which can differ from their merge commit).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Observed {
    Push {
        repository: String,
        branch_ref: String,
        head: String,
    },
    PullRequest {
        repository: String,
        branch_ref: String,
        base_ref: String,
        head: String,
        number: u64,
        title: String,
        body: String,
        open: bool,
    },
    Queue {
        repository: String,
        branch_ref: String,
        base_ref: String,
        head: String,
        number: u64,
        status: QueueStatus,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueueStatus {
    Queued,
    Merged,
    Removed,
}

impl DeliveryRemote {
    /// Protected integration branch checked by the source trial merge.
    pub(crate) fn base_ref(&self) -> &str {
        &self.target.base_ref
    }
    pub(crate) fn open(state: &ProjectState) -> Result<Self> {
        let private = state.private_dir();
        ensure!(
            private.is_absolute() && !private.starts_with(state.root()),
            "delivery settings must be outside the project checkout"
        );
        Self::from_private(&private)
    }

    fn from_private(private: &Path) -> Result<Self> {
        protected(private, true)?;
        let dir = private.join("delivery");
        protected(&dir, true)?;
        protected(&dir.join("adapter"), false)?;
        let config = dir.join("target.json");
        protected(&config, false)?;
        let mut bytes = Vec::new();
        File::open(config)?
            .take((WIRE_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= WIRE_LIMIT,
            "delivery target configuration exceeds its bound"
        );
        let target: DeliveryTarget =
            serde_json::from_slice(&bytes).context("invalid protected delivery target")?;
        target.validate()?;
        Ok(Self { dir, target })
    }

    /// Build the exact proposal for display to the operator. The host supplies
    /// the source identity and approved candidate SHA after validating evidence.
    pub(crate) fn plan(
        &self,
        project: &str,
        source_run: &str,
        occurrence: &str,
        head: &str,
        trial_ref: Option<&str>,
        trial_sha: Option<&str>,
    ) -> Result<DeliveryPlan> {
        ensure!(is_sha(head), "delivery needs a full candidate commit SHA");
        ensure!(
            trial_ref.is_some() == trial_sha.is_some(),
            "delivery trial identity is incomplete"
        );
        if let Some(checked_ref) = trial_ref {
            ensure!(
                checked_ref == self.base_ref(),
                "delivery base differs from checked trial ref"
            );
        }
        if let Some(checked_sha) = trial_sha {
            ensure!(
                is_sha(checked_sha),
                "delivery needs a full checked trial SHA"
            );
        }
        let target = &self.target;
        let mut push =
            json!({"repository":target.repository,"branch_ref":target.branch_ref,"head":head});
        push["expected_old"] = json!(target.expected_old);
        let enqueue = json!({"repository":target.repository,"branch_ref":target.branch_ref,"base_ref":target.base_ref,"head":head});
        let mut pull_request = enqueue.clone();
        pull_request["expected_number"] = json!(target.pull_request_number);
        pull_request["title"] = json!(target.title);
        pull_request["body"] = json!(target.body);
        Ok(DeliveryPlan {
            project: project.into(),
            source_run: source_run.into(),
            occurrence: occurrence.into(),
            trial_ref: trial_ref.map(str::to_owned),
            trial_sha: trial_sha.map(str::to_owned),
            push,
            pull_request,
            enqueue,
        })
    }

    /// Send once under a new ledger dispatch permit. Any error may follow a
    /// remote mutation and must be reconciled by inspecting the original key.
    pub(crate) async fn apply(
        &self,
        plan: &DeliveryPlan,
        slot: DeliverySlot,
        key: &str,
        prior_pr_receipt: Option<&Value>,
    ) -> Result<Value> {
        self.invoke(Method::Apply, plan, slot, key, prior_pr_receipt)
            .await?
            .ok_or_else(|| anyhow!("delivery adapter did not confirm the approved operation"))
    }

    /// A positive match is a receipt. Missing, mismatched or unreadable state
    /// never offers another permit to send.
    pub(crate) async fn inspect(
        &self,
        plan: &DeliveryPlan,
        slot: DeliverySlot,
        key: &str,
        prior_pr_receipt: Option<&Value>,
    ) -> Result<Option<Value>> {
        self.invoke(Method::Inspect, plan, slot, key, prior_pr_receipt)
            .await
    }

    async fn invoke(
        &self,
        method: Method,
        plan: &DeliveryPlan,
        slot: DeliverySlot,
        key: &str,
        prior_pr_receipt: Option<&Value>,
    ) -> Result<Option<Value>> {
        let head = plan.push["head"]
            .as_str()
            .context("missing approved delivery SHA")?;
        ensure!(
            self.plan(
                &plan.project,
                &plan.source_run,
                &plan.occurrence,
                head,
                plan.trial_ref.as_deref(),
                plan.trial_sha.as_deref()
            )? == *plan,
            "protected delivery target differs from the approved plan"
        );
        ensure!(
            key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid provider operation key"
        );
        let (request, number) = match slot {
            DeliverySlot::Push => (&plan.push, None),
            DeliverySlot::PullRequest => (&plan.pull_request, None),
            DeliverySlot::Enqueue => {
                let prior =
                    prior_pr_receipt.context("queue requires a recorded pull request receipt")?;
                let number = prior["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .context("invalid pull request receipt number")?;
                ensure!(
                    prior == &receipt(&plan.pull_request, Some(number)),
                    "queue receipt belongs to another pull request or revision"
                );
                (&plan.enqueue, Some(number))
            }
        };
        let input = serde_json::to_vec(
            &json!({"version":1,"method":method,"slot":slot,"key":key,"request":request,"number":number}),
        )?;
        ensure!(
            input.len() <= WIRE_LIMIT,
            "delivery provider request exceeds its bound"
        );
        let dir = self.dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            let adapter = dir.join("adapter");
            protected(&dir, true)?;
            protected(&adapter, false)?;
            let limits = Limits {
                time: Duration::from_secs(30),
                stdout: WIRE_LIMIT,
                stderr: 4096,
                cpu_seconds: Some(30),
                data_bytes: None,
            };
            let completed = process::run(&adapter, &[], Some(&dir), &[], &input, limits)
                .map_err(|_| anyhow!("could not obtain a bounded delivery provider response"))?;
            if !completed.success() {
                bail!("delivery provider did not complete successfully");
            }
            serde_json::from_slice::<Response>(&completed.stdout)
                .map_err(|_| anyhow!("malformed delivery provider response"))
        })
        .await
        .context("delivery provider worker stopped")??;
        ensure!(
            result.version == 1 && result.key == key,
            "delivery response does not match request version and key"
        );
        let Some(state) = result.state else {
            return Ok(None);
        };
        let target = &self.target;
        let observed = match state {
            Observed::Push {
                repository,
                branch_ref,
                head: seen,
            } if slot == DeliverySlot::Push
                && repository == target.repository
                && branch_ref == target.branch_ref
                && seen == head =>
            {
                receipt(request, None)
            }
            Observed::PullRequest {
                repository,
                branch_ref,
                base_ref,
                head: seen,
                number,
                title,
                body,
                open,
            } if slot == DeliverySlot::PullRequest
                && open
                && number > 0
                && target
                    .pull_request_number
                    .is_none_or(|expected| expected == number)
                && repository == target.repository
                && branch_ref == target.branch_ref
                && base_ref == target.base_ref
                && seen == head
                && title == target.title
                && body == target.body =>
            {
                receipt(request, Some(number))
            }
            Observed::Queue {
                repository,
                branch_ref,
                base_ref,
                head: queued_head,
                number: observed_number,
                status: QueueStatus::Queued | QueueStatus::Merged,
            } if slot == DeliverySlot::Enqueue
                && Some(observed_number) == number
                && repository == target.repository
                && branch_ref == target.branch_ref
                && base_ref == target.base_ref
                && queued_head == head =>
            {
                receipt(request, number)
            }
            _ => return Ok(None),
        };
        Ok(Some(observed))
    }
}

fn receipt(request: &Value, number: Option<u64>) -> Value {
    let mut value = request.clone();
    if let Some(number) = number {
        value["number"] = json!(number);
    }
    value
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::{SystemTime, UNIX_EPOCH};

    const PROVIDER: &[u8] = include_bytes!("../tests/support/delivery_remote.py");
    const HEAD: &str = "b1228db9b29124d5a071673ca8192f4d3a915377";
    const SECOND: &str = "e792c2f3194be3dff25dc7946d67bb2d91a0c925";

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "atomic-delivery-adapter-{}-{nonce}",
                std::process::id()
            ));
            let dir = path.join("delivery");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("adapter"), PROVIDER).unwrap();
            fs::set_permissions(dir.join("adapter"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(dir.join("target.json"), serde_json::to_vec(&json!({
                "repository":"trusted/repo", "branch_ref":"refs/heads/lane", "base_ref":"refs/heads/main",
                "expected_old":null, "title":"Reviewed change", "body":"Green checks",
            })).unwrap()).unwrap();
            Self(path)
        }

        fn remote(&self) -> DeliveryRemote {
            DeliveryRemote::from_private(&self.0).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn delivery_remote_reconciles_exact_push_pr_and_merged_queue_after_lost_replies() {
        let fixture = Fixture::new();
        let remote = fixture.remote();
        let dir = &remote.dir;
        let plan = remote
            .plan("project", "source", "fix", HEAD, None, None)
            .unwrap();
        let push = &"0".repeat(64);
        let pr = &"1".repeat(64);
        let enqueue = &"2".repeat(64);
        let slot = DeliverySlot::Push;
        fs::write(dir.join("drop-response-push"), "").unwrap();
        assert!(
            remote
                .inspect(&plan, slot, push, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(remote.apply(&plan, slot, push, None).await.is_err());
        fs::write(dir.join("unreadable-push"), "").unwrap();
        assert!(remote.inspect(&plan, slot, push, None).await.is_err());
        fs::remove_file(dir.join("unreadable-push")).unwrap();
        assert_eq!(
            remote.inspect(&plan, slot, push, None).await.unwrap(),
            Some(plan.push.clone())
        );
        let slot = DeliverySlot::PullRequest;
        let receipt = remote.apply(&plan, slot, pr, None).await.unwrap();
        assert_eq!(receipt["number"], 1);
        assert_eq!(
            remote.inspect(&plan, slot, pr, None).await.unwrap(),
            Some(receipt.clone())
        );
        let slot = DeliverySlot::Enqueue;
        let mut wrong = receipt.clone();
        wrong["head"] = json!(SECOND);
        assert!(
            remote
                .apply(&plan, slot, enqueue, Some(&wrong))
                .await
                .is_err()
        );
        let queued = remote
            .apply(&plan, slot, enqueue, Some(&receipt))
            .await
            .unwrap();
        let state = dir.join("remote-state.json");
        let mut s: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(
            s["apply_calls"],
            json!({"push":1,"pull_request":1,"enqueue":1})
        );
        assert_eq!(s["mutations"], s["apply_calls"]);
        let id = s["queue"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        s["queue"][&id]["status"] = json!("merged");
        fs::write(&state, serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(
            remote
                .inspect(&plan, slot, enqueue, Some(&receipt))
                .await
                .unwrap(),
            Some(queued)
        );
        s["queue"][&id]["head"] = json!(SECOND);
        fs::write(&state, serde_json::to_vec(&s).unwrap()).unwrap();
        assert!(
            remote
                .inspect(&plan, slot, enqueue, Some(&receipt))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delivery_remote_updates_existing_pull_request_and_rejects_changed_target() {
        let fixture = Fixture::new();
        let remote = fixture.remote();
        let first = remote
            .plan("project", "source", "fix", HEAD, None, None)
            .unwrap();
        remote
            .apply(&first, DeliverySlot::Push, &"0".repeat(64), None)
            .await
            .unwrap();
        let original = remote
            .apply(&first, DeliverySlot::PullRequest, &"1".repeat(64), None)
            .await
            .unwrap();
        let config = remote.dir.join("target.json");
        let mut target: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        target["expected_old"] = json!(HEAD);
        target["title"] = json!("Updated change");
        target["pull_request_number"] = json!(1);
        fs::write(&config, serde_json::to_vec(&target).unwrap()).unwrap();
        let updated = fixture.remote();
        assert!(
            updated
                .apply(&first, DeliverySlot::Push, &"2".repeat(64), None)
                .await
                .is_err()
        );
        let plan = updated
            .plan("project", "updated-run", "fix", SECOND, None, None)
            .unwrap();
        updated
            .apply(&plan, DeliverySlot::Push, &"3".repeat(64), None)
            .await
            .unwrap();
        let pr = updated
            .apply(&plan, DeliverySlot::PullRequest, &"4".repeat(64), None)
            .await
            .unwrap();
        assert_eq!(pr["number"], original["number"]);
        assert_eq!(pr["head"], SECOND);
        assert!(
            remote
                .inspect(&first, DeliverySlot::PullRequest, &"1".repeat(64), None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delivery_remote_refuses_symlinked_adapter_and_writable_config() {
        let fixture = Fixture::new();
        let dir = fixture.0.join("delivery");
        let target = dir.join("target.json");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(DeliveryRemote::from_private(&fixture.0).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(dir.join("adapter"), dir.join("elsewhere")).unwrap();
        symlink("elsewhere", dir.join("adapter")).unwrap();
        assert!(DeliveryRemote::from_private(&fixture.0).is_err());
    }
}
