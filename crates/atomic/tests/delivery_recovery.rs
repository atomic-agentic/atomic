// SPDX-License-Identifier: Apache-2.0

//! Recovery from a lost remote response. The seed process writes an admitted
//! delivery input and a finalized source item. Every recovery, gate decision
//! and park release runs through the actual CLI in its own OS process.

#![cfg(unix)]

use atomic_app::delivery_evidence::DeliverySource;
use atomic_app::pipeline::{GRAPH_PIPELINE, LAYOUT_VERSION};
use atomic_core::credential::{FileSecrets, SecretStore};
use atomic_core::state::ProjectState;
use atomic_durable::delivery_effect::{DeliveryPlan, WORKFLOW};
use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::publication::Acceptance;
use atomic_durable::{Database, DurableRuntime, Durare, DurareContext, Param, WorkflowContext};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const SOURCE: &str = "reviewed-source";
const RUN: &str = "delivery-crash-run";
const STEP: &str = "graph.00000.fix";
const HEAD: &str = "45f80e38d6c836b32d82ef185c152d69fc8dc383";
const TRIAL_HEAD: &str = "94cf16751ef4fa8f0436bbb3cab9dbac05082867";
const OWNER: &str = "delivery-owner";
const CANDIDATE: &str = "source-candidate";
const DIGEST: &str = "source-digest";

fn text(s: &str) -> Param {
    Param::Text(s.into())
}

fn project(root: &Path) -> PathBuf {
    root.join("project")
}

fn command(root: &Path, path: &Path) -> Command {
    let mut cmd = Command::new(path);
    cmd.current_dir(project(root))
        .env("ATOMIC_HOME", root.join("state"))
        .env("HOME", root.join("home"))
        .env("USER", OWNER)
        .env_remove("LOGNAME")
        .env("ATOMIC_CREDENTIAL_STORE", "file")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("ATOMIC_DATABASE_URL")
        .env_remove("ATOMIC_FAILPOINT")
        .stdin(Stdio::null());
    cmd
}

fn cli(root: &Path, args: &[&str], failpoint: Option<&str>) -> Output {
    let mut cmd = command(root, Path::new(env!("CARGO_BIN_EXE_atomic")));
    cmd.arg("--json").args(args);
    if let Some(name) = failpoint {
        cmd.env("ATOMIC_FAILPOINT", name);
    }
    cmd.output().unwrap()
}

fn success(out: Output) -> Vec<u8> {
    assert!(out.status.success(), "command failed: {out:?}");
    out.stdout
}

fn resume(root: &Path, failpoint: Option<&str>) -> Output {
    cli(root, &["delivery", "resume", RUN], failpoint)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "seed subprocess used by the delivery recovery test"]
async fn seed_admitted_delivery() {
    let root = PathBuf::from(std::env::var_os("DELIVERY_RECOVERY_ROOT").unwrap());
    let state = ProjectState::resolve(&project(&root)).unwrap();
    state.create().unwrap();
    let id = format!("project/{}", state.id());
    let mut rt = Durare::open(
        &Database::Sqlite(state.database()),
        None,
        LAYOUT_VERSION,
        &id,
    )
    .await
    .unwrap();
    // Enter the store through the real durable APIs. This fixture starts
    // downstream of source admission and exercises the production source
    // revalidation against committed acceptance, publication and close rows.
    rt.register(GRAPH_PIPELINE, |ctx: DurareContext, _: ()| async move {
        ctx.step(&format!("{STEP}.trial_merge.0"), |_| async {
            Ok(json!({"Passed":{"main":TRIAL_HEAD}}))
        }).await?;
        ctx.step(&format!("{STEP}.accepted"), |_| async {
            Ok(Acceptance {candidate:CANDIDATE.into(),digest:DIGEST.into(),accepted:true,reason:Some("approved".into())})
        }).await?;
        ctx.transaction(&format!("{STEP}.finalize.0"), |mut tx| Box::pin(async move {
            tx.execute("INSERT INTO atomic_publications (publication_key,digest,output) VALUES (?,?,?)",
                &[text(CANDIDATE),text(DIGEST),text("true")]).await?;
            tx.execute("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,closed_at,terminal_outcome,last_finish_claimant,last_finish_session,last_finish_epoch,last_finish_acquisition) VALUES ('item','item','','task',1,'closed','epic','human','owner',1,2,'success',?,1,1,'source-first-claim')", &[text(SOURCE)]).await?;
            tx.execute("INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch) VALUES ('item',?,0,0)", &[text(HEAD)]).await?;
            Ok(json!({"status":"applied","output":true}))
        })).await?;
        Ok(())
    });
    // Leave the delivery pending at position zero for the actual CLI worker.
    rt.register(WORKFLOW, |_: DurareContext, _: Value| {
        std::future::pending::<atomic_durable::Result<()>>()
    });
    rt.launch().await.unwrap();
    rt.start(GRAPH_PIPELINE, SOURCE, ()).await.unwrap();
    rt.result::<()>(SOURCE).await.unwrap();
    let credential = HumanCredential::new(Principal::local(OWNER), "delivery operator secret");
    gate::enroll(&rt, &credential).await.unwrap();
    let files = FileSecrets::open(&root.join("state/credentials"), &project(&root)).unwrap();
    files
        .save(
            &format!("operator:{OWNER}@{}", state.id()),
            "delivery operator secret",
        )
        .unwrap();
    let dir = state.private_dir().join("delivery");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("adapter"),
        include_bytes!("support/delivery_remote.py"),
    )
    .unwrap();
    fs::set_permissions(dir.join("adapter"), fs::Permissions::from_mode(0o700)).unwrap();
    let target = json!({"repository":"test/repo","branch_ref":"refs/heads/lane","expected_old":null,
        "base_ref":"refs/heads/main","title":"Reviewed","body":"Green"});
    fs::write(
        dir.join("target.json"),
        serde_json::to_vec(&target).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("drop-response-push"), "").unwrap();
    fs::write(dir.join("unreadable-push"), "").unwrap();
    let push = json!({"repository":target["repository"],"branch_ref":target["branch_ref"],"head":HEAD,"expected_old":null});
    let enqueue = json!({"repository":target["repository"],"branch_ref":target["branch_ref"],"base_ref":target["base_ref"],"head":HEAD});
    let mut pull_request = enqueue.clone();
    pull_request["expected_number"] = Value::Null;
    pull_request["title"] = target["title"].clone();
    pull_request["body"] = target["body"].clone();
    let plan = DeliveryPlan {
        project: id.clone(),
        source_run: SOURCE.into(),
        occurrence: format!("{STEP}:0"),
        trial_ref: Some("refs/heads/main".into()),
        trial_sha: Some(TRIAL_HEAD.into()),
        push,
        pull_request,
        enqueue,
    };
    let source = DeliverySource {
        project: Some(id),
        source_run: SOURCE.into(),
        epic_id: "epic".into(),
        base_step: STEP.into(),
        item_id: "item".into(),
        head: HEAD.into(),
        candidate_id: CANDIDATE.into(),
        digest: DIGEST.into(),
        occurrence: 0,
        review_round: 0,
        lease_session: 1,
        lease_epoch: 1,
        lease_acquisition: "source-first-claim".into(),
        trial_main_ref: "refs/heads/main".into(),
        trial_checked_sha: TRIAL_HEAD.into(),
    };
    rt.start(
        WORKFLOW,
        RUN,
        json!({"plan":plan,"operator":OWNER,"source":source}),
    )
    .await
    .unwrap();
    fs::write(root.join("state-id"), state.id()).unwrap();
    // End this dedicated seed worker before it could execute a delivery step.
    std::process::exit(0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_visibility_waits_for_operator_release_and_recovery_dispatches_each_effect_once() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "atomic-delivery-recovery-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(project(&root)).unwrap();
    fs::create_dir(root.join("home")).unwrap();
    let seed = command(&root, &std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "seed_admitted_delivery",
            "--nocapture",
        ])
        .env("DELIVERY_RECOVERY_ROOT", &root)
        .output()
        .unwrap();
    success(seed);
    let id = fs::read_to_string(root.join("state-id")).unwrap();
    let private = root.join("state/private").join(&id);
    let dir = private.join("delivery");
    let rt = Durare::open(
        &Database::Sqlite(private.join("atomic.db")),
        None,
        LAYOUT_VERSION,
        &format!("project/{id}"),
    )
    .await
    .unwrap();
    let open: Value = serde_json::from_slice(&success(resume(&root, None))).unwrap();
    let gate = open["gate"].as_str().expect("open delivery gate");
    assert!(
        !dir.join("remote-state.json").exists(),
        "effect preceded approval"
    );
    success(cli(
        &root,
        &["gates", "approve", gate, "--command-id", "approve-delivery"],
        None,
    ));
    let first = resume(&root, Some("delivery:after-decision"));
    assert_eq!(
        first.status.signal(),
        Some(9),
        "did not SIGKILL after unknown checkpoint: {first:?}"
    );
    let steps = rt.steps(RUN).await.unwrap();
    let decision = steps
        .iter()
        .find(|s| s.name == "delivery.push.decision.0")
        .unwrap();
    assert_eq!(decision.output.as_ref(), Some(&json!("Unknown")));
    let fake = || {
        serde_json::from_slice::<Value>(&fs::read(dir.join("remote-state.json")).unwrap()).unwrap()
    };
    assert_eq!(fake()["apply_calls"], json!({"push":1}));
    assert_eq!(fake()["mutations"], json!({"push":1}));
    fs::remove_file(dir.join("unreadable-push")).unwrap();
    let waiting: Value = serde_json::from_slice(&success(resume(&root, None))).unwrap();
    assert_eq!(waiting["status"], "outcome_unknown");
    let park = waiting["park"].as_str().expect("durable unknown park");
    assert_eq!(fake()["apply_calls"], json!({"push":1}));
    assert!(
        !rt.steps(RUN)
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "delivery.push.done")
    );
    success(cli(
        &root,
        &[
            "parks",
            "release",
            park,
            "--command-id",
            "release-reconciled-push",
        ],
        None,
    ));
    let released = resume(&root, Some("delivery:after-park-release"));
    assert_eq!(
        released.status.signal(),
        Some(9),
        "did not SIGKILL after human release: {released:?}"
    );
    assert!(
        !rt.steps(RUN)
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "delivery.push.decision.1")
    );
    let final_output: Value = serde_json::from_slice(&success(resume(&root, None))).unwrap();
    assert_eq!(final_output["effects"].as_array().unwrap().len(), 3);
    let steps = rt.steps(RUN).await.unwrap();
    assert_eq!(
        steps
            .iter()
            .filter(|s| s.name == "delivery.push.decision.0")
            .count(),
        1
    );
    assert!(steps.iter().any(|s| s.name == "delivery.push.decision.1"
        && s.output.as_ref().and_then(|x| x.get("Applied")).is_some()));
    let state = fake();
    assert_eq!(
        state["apply_calls"],
        json!({"push":1,"pull_request":1,"enqueue":1})
    );
    assert_eq!(state["mutations"], state["apply_calls"]);
    assert_eq!(final_output["effects"][1]["receipt"]["number"], 1);
    assert_eq!(final_output["effects"][2]["receipt"]["number"], 1);
    assert_eq!(
        gate::gates(
            &rt,
            gate::GateFilter {
                open_only: false,
                run: Some(RUN.into())
            }
        )
        .await
        .unwrap()
        .len(),
        1
    );
    drop(rt);
    fs::remove_dir_all(root).unwrap();
}
