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
    cmd.output().expect("run atomic CLI child")
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
    let root = PathBuf::from(std::env::var_os("DELIVERY_RECOVERY_ROOT").expect("seed root"));
    let state = ProjectState::resolve(&project(&root)).expect("resolve seed project");
    state.create().expect("create seed project state");
    let id = format!("project/{}", state.id());
    let mut rt = Durare::open(
        &Database::Sqlite(state.database()),
        None,
        LAYOUT_VERSION,
        &id,
    )
    .await
    .expect("open seed durable runtime");
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
    rt.launch().await.expect("launch seed runtime");
    rt.start(GRAPH_PIPELINE, SOURCE, ())
        .await
        .expect("start reviewed source");
    rt.result::<()>(SOURCE)
        .await
        .expect("finish reviewed source");
    let credential = HumanCredential::new(Principal::local(OWNER), "delivery operator secret");
    gate::enroll(&rt, &credential)
        .await
        .expect("enroll delivery operator");
    let files = FileSecrets::open(&root.join("state/credentials"), &project(&root))
        .expect("open operator credentials");
    files
        .save(
            &format!("operator:{OWNER}@{}", state.id()),
            "delivery operator secret",
        )
        .expect("save delivery operator credential");
    let dir = state.private_dir().join("delivery");
    fs::create_dir(&dir).expect("create delivery fixture directory");
    fs::write(
        dir.join("adapter"),
        include_bytes!("support/delivery_remote.py"),
    )
    .expect("install fake remote adapter");
    fs::set_permissions(dir.join("adapter"), fs::Permissions::from_mode(0o700))
        .expect("make fake remote executable");
    let target = json!({"repository":"test/repo","branch_ref":"refs/heads/lane","expected_old":null,
        "base_ref":"refs/heads/main","title":"Reviewed","body":"Green"});
    fs::write(
        dir.join("target.json"),
        serde_json::to_vec(&target).expect("encode delivery target"),
    )
    .expect("write delivery target");
    fs::write(dir.join("drop-response-push"), "").expect("drop push response");
    fs::write(dir.join("unreadable-push"), "").expect("block push inspection");
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
    .expect("start admitted delivery");
    fs::write(root.join("state-id"), state.id()).expect("record seed state id");
    // End this dedicated seed worker before it could execute a delivery step.
    std::process::exit(0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_visibility_waits_for_operator_release_and_recovery_dispatches_each_effect_once() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time follows Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "atomic-delivery-recovery-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(project(&root)).expect("create recovery project");
    fs::create_dir(root.join("home")).expect("create recovery home");
    let seed = command(&root, &std::env::current_exe().expect("test binary path"))
        .args([
            "--ignored",
            "--exact",
            "seed_admitted_delivery",
            "--nocapture",
        ])
        .env("DELIVERY_RECOVERY_ROOT", &root)
        .output()
        .expect("start seed child");
    success(seed);
    let id = fs::read_to_string(root.join("state-id")).expect("read seed state id");
    let private = root.join("state/private").join(&id);
    let dir = private.join("delivery");
    let rt = Durare::open(
        &Database::Sqlite(private.join("atomic.db")),
        None,
        LAYOUT_VERSION,
        &format!("project/{id}"),
    )
    .await
    .expect("open recovery ledger");
    let open: Value =
        serde_json::from_slice(&success(resume(&root, None))).expect("parse opened delivery gate");
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
    let steps = rt.steps(RUN).await.expect("read first delivery steps");
    let decision = steps
        .iter()
        .find(|s| s.name == "delivery.push.decision.0")
        .expect("unknown push decision");
    assert_eq!(decision.output.as_ref(), Some(&json!("Unknown")));
    let fake = || {
        serde_json::from_slice::<Value>(
            &fs::read(dir.join("remote-state.json")).expect("read fake remote state"),
        )
        .expect("parse fake remote state")
    };
    assert_eq!(fake()["apply_calls"], json!({"push":1}));
    assert_eq!(fake()["mutations"], json!({"push":1}));
    fs::remove_file(dir.join("unreadable-push")).expect("restore push inspection");
    let waiting: Value =
        serde_json::from_slice(&success(resume(&root, None))).expect("parse uncertainty park");
    assert_eq!(waiting["status"], "outcome_unknown");
    let park = waiting["park"].as_str().expect("durable unknown park");
    assert_eq!(fake()["apply_calls"], json!({"push":1}));
    assert!(
        !rt.steps(RUN)
            .await
            .expect("read parked delivery steps")
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
            .expect("read released delivery steps")
            .iter()
            .any(|s| s.name == "delivery.push.decision.1")
    );
    let final_output: Value = serde_json::from_slice(&success(resume(&root, None)))
        .expect("parse completed delivery result");
    assert_eq!(
        final_output["effects"]
            .as_array()
            .expect("three delivered effects")
            .len(),
        3
    );
    let steps = rt.steps(RUN).await.expect("read completed delivery steps");
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
        .expect("read delivery gates")
        .len(),
        1
    );
    drop(rt);
    fs::remove_dir_all(root).expect("remove delivery recovery fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_delivery_effect_boundary_recovers_without_repeating_remote_mutations() {
    for site in [
        "delivery:after-gate",
        "delivery:after-intent",
        "delivery:after-dispatch",
        "delivery:after-record",
    ] {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time follows Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "atomic-delivery-{site}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(project(&root)).expect("create test project");
        fs::create_dir(root.join("home")).expect("create test home");
        let seed = command(&root, &std::env::current_exe().expect("test binary path"))
            .args([
                "--ignored",
                "--exact",
                "seed_admitted_delivery",
                "--nocapture",
            ])
            .env("DELIVERY_RECOVERY_ROOT", &root)
            .output()
            .expect("start seed process");
        success(seed);
        let id = fs::read_to_string(root.join("state-id")).expect("read project id");
        let private = root.join("state/private").join(&id);
        let dir = private.join("delivery");
        let rt = Durare::open(
            &Database::Sqlite(private.join("atomic.db")),
            None,
            LAYOUT_VERSION,
            &format!("project/{id}"),
        )
        .await
        .expect("open delivery ledger");
        let open: Value =
            serde_json::from_slice(&success(resume(&root, None))).expect("open gate output");
        let gate = open["gate"].as_str().expect("open delivery gate");
        assert!(!dir.join("remote-state.json").exists(), "{site}");
        success(cli(
            &root,
            &["gates", "approve", gate, "--command-id", "approve-delivery"],
            None,
        ));
        // This series exercises successful remote responses. The original
        // fixture's dropped response and unreadable inspection are covered by
        // the separate outcome_unknown test above.
        fs::remove_file(dir.join("drop-response-push")).expect("enable push response");
        fs::remove_file(dir.join("unreadable-push")).expect("enable push inspection");

        let killed = resume(&root, Some(site));
        assert_eq!(killed.status.signal(), Some(9), "{site}: {killed:?}");
        if site == "delivery:after-gate" || site == "delivery:after-intent" {
            assert!(!dir.join("remote-state.json").exists(), "{site}");
        } else {
            let state: Value = serde_json::from_slice(
                &fs::read(dir.join("remote-state.json")).expect("read fake remote"),
            )
            .expect("parse fake remote");
            assert_eq!(state["mutations"], json!({"push":1}), "{site}");
        }

        let resumed: Value =
            serde_json::from_slice(&success(resume(&root, None))).expect("recovery output");
        if site == "delivery:after-intent" {
            assert_eq!(resumed["status"], "outcome_unknown", "{site}: {resumed}");
            let park = resumed["park"].as_str().expect("intent uncertainty park");
            let state: Value = serde_json::from_slice(
                &fs::read(dir.join("remote-state.json")).expect("read inspection state"),
            )
            .expect("parse inspection state");
            assert_eq!(state["mutations"], json!({}), "{site}");
            assert_eq!(state["apply_calls"], json!({}), "{site}");
            assert!(
                !rt.steps(RUN)
                    .await
                    .expect("read pending steps")
                    .iter()
                    .any(|step| step.name == "delivery.push.done"),
                "{site} silently completed an uncertain push"
            );
            success(cli(
                &root,
                &["parks", "release", park, "--command-id", "release-intent"],
                None,
            ));
            let still_uncertain: Value = serde_json::from_slice(&success(resume(&root, None)))
                .expect("rechecked intent output");
            assert_eq!(
                still_uncertain["status"], "outcome_unknown",
                "{site}: release without remote evidence must not dispatch again"
            );
            let state: Value = serde_json::from_slice(
                &fs::read(dir.join("remote-state.json")).expect("read reinspection state"),
            )
            .expect("parse reinspection state");
            assert_eq!(state["mutations"], json!({}), "{site}");
            assert_eq!(state["apply_calls"], json!({}), "{site}");
            assert!(
                !rt.steps(RUN)
                    .await
                    .expect("read rechecked steps")
                    .iter()
                    .any(|step| step.name == "delivery.push.done"),
                "{site} silently completed an uncertain push"
            );
            drop(rt);
            fs::remove_dir_all(root).expect("remove uncertain fixture");
            continue;
        }
        let finished = resumed;
        assert_eq!(
            finished["effects"].as_array().map(Vec::len),
            Some(3),
            "{site}"
        );
        let state: Value = serde_json::from_slice(
            &fs::read(dir.join("remote-state.json")).expect("read completed fake remote"),
        )
        .expect("parse completed fake remote");
        assert_eq!(
            state["mutations"],
            json!({"push":1,"pull_request":1,"enqueue":1}),
            "{site}"
        );
        assert_eq!(state["apply_calls"], state["mutations"], "{site}");
        assert_eq!(
            state["keys"].as_object().map(|keys| keys.len()),
            Some(3),
            "{site}"
        );
        assert_eq!(finished["effects"][1]["receipt"]["number"], 1, "{site}");
        assert_eq!(finished["effects"][2]["receipt"]["number"], 1, "{site}");
        drop(rt);
        fs::remove_dir_all(root).expect("remove recovery fixture");
    }
}
