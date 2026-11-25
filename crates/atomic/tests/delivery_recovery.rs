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
const OLD_LAYOUT_RUN: &str = "a-old-layout-delivery";
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
        evidence_run: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_polls_queue_after_restart_without_repeating_delivery() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let scratch_base = PathBuf::from(std::env::var_os("HOME").expect("test home"));
    let root = scratch_base.join(format!(
        "atomic-delivery-poller-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(project(&root)).expect("create project");
    fs::write(
        project(&root).join("atomic.yaml"),
        "atomic: 1\nname: project\n",
    )
    .expect("create project manifest for serve");
    fs::create_dir(root.join("home")).expect("create home");
    success(
        command(&root, &std::env::current_exe().expect("test binary"))
            .args([
                "--ignored",
                "--exact",
                "seed_admitted_delivery",
                "--nocapture",
            ])
            .env("DELIVERY_RECOVERY_ROOT", &root)
            .output()
            .expect("seed"),
    );
    let model_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake model");
    let model_url = format!(
        "http://{}/v1",
        model_listener.local_addr().expect("model address")
    );
    model_listener
        .set_nonblocking(true)
        .expect("nonblocking fake model");
    let model_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = model_calls.clone();
    let stop = model_stop.clone();
    let model_worker = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match model_listener.accept() {
                Ok((_stream, _)) => {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("fake model listener failed: {error}"),
            }
        }
    });
    let id = fs::read_to_string(root.join("state-id")).expect("project ID");
    let dir = root.join("state/private").join(&id).join("delivery");
    let gate_output: Value = serde_json::from_slice(&success(resume(&root, None))).expect("gate");
    let gate = gate_output["gate"].as_str().expect("gate ID");
    success(cli(
        &root,
        &["gates", "approve", gate, "--command-id", "approve-delivery"],
        None,
    ));
    fs::remove_file(dir.join("drop-response-push")).expect("allow push");
    fs::remove_file(dir.join("unreadable-push")).expect("allow inspection");
    success(resume(&root, None));
    let db = root.join("state/private").join(&id).join("atomic.db");
    let rt = Durare::open(
        &Database::Sqlite(db),
        None,
        LAYOUT_VERSION,
        &format!("project/{id}"),
    )
    .await
    .expect("open store");
    // Make a pending row due without waiting for the production cadence.
    let due = || {
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                tx.execute(
                    "UPDATE atomic_delivery_queue_observations SET next_poll_at=0 WHERE delivery_run=?",
                    &[text(RUN)],
                )
                .await?;
                Ok(())
            })
        })
    };
    let status = || {
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                let row = tx.query_one(
                    "SELECT status,poll_attempts,next_poll_at FROM atomic_delivery_queue_observations WHERE delivery_run=?",
                    &[text(RUN)],
                ).await?;
                Ok((row.text("status")?, row.int("poll_attempts")?, row.int("next_poll_at")?))
            })
        })
    };
    fs::write(dir.join("unreadable-enqueue"), "").expect("fail provider read");
    due().await.expect("make failed read due");
    let project_key = format!("project/{id}");
    rt.control_transaction(move |mut tx| {
        let project = project_key.clone();
        Box::pin(async move {
            tx.execute(
                "INSERT INTO workflow_status (workflow_uuid,status,name,application_version,inputs,output) SELECT ?,status,name,?,inputs,output FROM workflow_status WHERE workflow_uuid=?",
                &[text(OLD_LAYOUT_RUN), text("atomic.pipeline.0"), text(RUN)],
            ).await?;
            tx.execute(
                "INSERT INTO atomic_run_projects (run_id,project) VALUES (?,?)",
                &[text(OLD_LAYOUT_RUN), text(&project)],
            ).await?;
            tx.execute(
                "INSERT INTO atomic_delivery_queue_observations (delivery_run,plan_digest,repository,pull_request_number,head,status,provider_event_id,merge_sha,happened_at,observed_at,detail,poll_attempts,next_poll_at) SELECT ?,plan_digest,repository,pull_request_number,head,'pending',NULL,NULL,NULL,observed_at,'',0,0 FROM atomic_delivery_queue_observations WHERE delivery_run=?",
                &[text(OLD_LAYOUT_RUN), text(RUN)],
            ).await?;
            Ok(())
        })
    }).await.expect("seed old-layout due row");
    assert_eq!(
        rt.own_run_record(OLD_LAYOUT_RUN)
            .await
            .expect("old run")
            .expect("old run record")
            .version,
        "atomic.pipeline.0"
    );
    let own_project = format!("project/{id}");
    let due_run = rt
        .control_transaction(move |mut tx| {
            let project = own_project.clone();
            Box::pin(async move {
                atomic_durable::delivery_effect::due_queue_observation_in(
                    &mut tx,
                    &project,
                    i64::MAX,
                )
                .await
            })
        })
        .await
        .expect("query current project");
    assert_eq!(due_run.as_deref(), Some(OLD_LAYOUT_RUN));
    // Count model transport connections while the provider status changes.
    let mut first = command(&root, Path::new(env!("CARGO_BIN_EXE_atomic")))
        .arg("serve")
        .env("ATOMIC_MODEL_BASE_URL", &model_url)
        .env("ATOMIC_MODEL", "unused")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve first");
    let mut failed = false;
    for _ in 0..100 {
        if status().await.expect("read status").0 == "failed" {
            failed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if failed {
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let remote: Value = serde_json::from_slice(
            &fs::read(dir.join("remote-state.json")).expect("read failed provider state"),
        )
        .expect("parse failed provider state");
        assert_eq!(
            remote["observe_calls"]["enqueue"], 2,
            "backoff must prevent an immediate retry"
        );
    }
    first.kill().expect("kill first serve");
    let first_output = first.wait_with_output().expect("reap first serve");
    assert!(
        failed,
        "serve never recorded unavailable provider: {first_output:?}"
    );
    let (state, attempts, next) = status().await.expect("failed schedule");
    assert_eq!(state, "failed");
    assert_eq!(attempts, 1);
    let (old_state, old_attempts, old_next) = rt.control_transaction(|mut tx| Box::pin(async move {
        let row = tx.query_one("SELECT status,poll_attempts,next_poll_at FROM atomic_delivery_queue_observations WHERE delivery_run=?", &[text(OLD_LAYOUT_RUN)]).await?;
        Ok((row.text("status")?, row.int("poll_attempts")?, row.int("next_poll_at")?))
    })).await.expect("old row schedule");
    assert_eq!(
        old_state, "pending",
        "incompatible row retains its last provider observation"
    );
    assert_eq!(old_attempts, 1, "incompatible row must be deferred durably");
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("stored time range");
    assert!(next > now + 5_000, "failure must schedule backoff");
    assert!(
        old_next > now + 5_000,
        "old-layout row must not stay first after restart"
    );
    let wrong_project = rt
        .control_transaction(move |mut tx| {
            Box::pin(async move {
                atomic_durable::delivery_effect::due_queue_observation_in(
                    &mut tx,
                    "project/other",
                    now,
                )
                .await
            })
        })
        .await
        .expect("query other project");
    assert!(
        wrong_project.is_none(),
        "poller must stay in the current project"
    );
    fs::remove_file(dir.join("unreadable-enqueue")).expect("restore provider");
    let state_path = dir.join("remote-state.json");
    let mut remote: Value = serde_json::from_slice(&fs::read(&state_path).expect("provider state"))
        .expect("parse provider");
    let key = remote["queue"]
        .as_object()
        .expect("queue")
        .keys()
        .next()
        .expect("queued PR")
        .clone();
    remote["queue"][&key]["status"] = json!("merged");
    remote["queue"][&key]["event_id"] = json!("queue:merged:restart");
    remote["queue"][&key]["merge_sha"] = json!(TRIAL_HEAD);
    remote["queue"][&key]["happened_at"] = json!(1_800_000_000_000_i64);
    fs::write(
        &state_path,
        serde_json::to_vec(&remote).expect("encode event"),
    )
    .expect("transition provider");
    due().await.expect("make recovery read due");
    let mut second = command(&root, Path::new(env!("CARGO_BIN_EXE_atomic")))
        .arg("serve")
        .env("ATOMIC_MODEL_BASE_URL", &model_url)
        .env("ATOMIC_MODEL", "unused")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve second");
    let mut merged = false;
    for _ in 0..100 {
        if status().await.expect("read status").0 == "merged" {
            merged = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    second.kill().expect("kill second serve");
    let second_output = second.wait_with_output().expect("reap second serve");
    assert!(
        merged,
        "serve never recorded terminal provider event: {second_output:?}"
    );
    let final_state: Value =
        serde_json::from_slice(&fs::read(&state_path).expect("final provider state"))
            .expect("parse final provider");
    let old_attempts_after_restart = rt.control_transaction(|mut tx| Box::pin(async move {
        tx.query_one("SELECT poll_attempts FROM atomic_delivery_queue_observations WHERE delivery_run=?", &[text(OLD_LAYOUT_RUN)]).await?.int("poll_attempts")
    })).await.expect("old row after restart");
    assert_eq!(
        old_attempts_after_restart, 1,
        "restart must honor persisted deferral"
    );
    assert_eq!(
        final_state["mutations"],
        json!({"push":1,"pull_request":1,"enqueue":1})
    );
    assert_eq!(
        final_state["observe_calls"]["enqueue"], 3,
        "one initial and one read per serve"
    );
    model_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    model_worker.join().expect("reap fake model");
    assert_eq!(
        model_calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "polling must not call a model"
    );
    due().await.expect("try to make terminal row due");
    let project = format!("project/{id}");
    let due_terminal = rt
        .control_transaction(move |mut tx| {
            let project = project.clone();
            Box::pin(async move {
                atomic_durable::delivery_effect::due_queue_observation_in(&mut tx, &project, now)
                    .await
            })
        })
        .await
        .expect("query terminal");
    assert!(
        due_terminal.is_none(),
        "terminal observation must never be polled"
    );
    drop(rt);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_observation_records_pending_failure_merge_and_drop_once_after_restart() {
    for final_status in ["merged", "removed"] {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time follows Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "atomic-delivery-observe-{final_status}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(project(&root)).expect("create observation project");
        fs::create_dir(root.join("home")).expect("create observation home");
        success(
            command(&root, &std::env::current_exe().expect("test binary path"))
                .args([
                    "--ignored",
                    "--exact",
                    "seed_admitted_delivery",
                    "--nocapture",
                ])
                .env("DELIVERY_RECOVERY_ROOT", &root)
                .output()
                .expect("start observation seed"),
        );
        let id = fs::read_to_string(root.join("state-id")).expect("read project ID");
        let dir = root.join("state/private").join(&id).join("delivery");
        let premature = cli(&root, &["delivery", "observe", RUN], None);
        assert!(
            !premature.status.success(),
            "observation ran before enqueue"
        );
        let refused: Value =
            serde_json::from_slice(&premature.stdout).expect("parse early observation refusal");
        assert_eq!(
            refused["error"]["description"],
            "delivery requires this run's human-approved operator gate",
            "wrong early-observation refusal: {premature:?}"
        );
        assert!(!dir.join("remote-state.json").exists());
        let open: Value =
            serde_json::from_slice(&success(resume(&root, None))).expect("parse delivery gate");
        let gate = open["gate"].as_str().expect("delivery gate ID");
        success(cli(
            &root,
            &["gates", "approve", gate, "--command-id", "approve-delivery"],
            None,
        ));
        fs::remove_file(dir.join("drop-response-push")).expect("enable push response");
        fs::remove_file(dir.join("unreadable-push")).expect("enable push inspection");
        if final_status == "removed" {
            fs::write(dir.join("unreadable-enqueue"), "")
                .expect("make initial queue observation unreadable");
        }
        if final_status == "merged" {
            let killed = resume(&root, Some("delivery:after-observation"));
            assert_eq!(
                killed.status.signal(),
                Some(9),
                "initial queue observation did not checkpoint before process kill: {killed:?}"
            );
            let state_path = dir.join("remote-state.json");
            let mut state: Value = serde_json::from_slice(
                &fs::read(&state_path).expect("read queued provider state after kill"),
            )
            .expect("parse queued provider state after kill");
            let key = state["queue"]
                .as_object()
                .expect("queue map after kill")
                .keys()
                .next()
                .expect("queued PR after kill")
                .clone();
            state["queue"][&key]["status"] = json!("merged");
            state["queue"][&key]["event_id"] = json!("queue:merged:1");
            state["queue"][&key]["merge_sha"] = json!(TRIAL_HEAD);
            state["queue"][&key]["happened_at"] = json!(1_800_000_000_000_i64);
            fs::write(
                state_path,
                serde_json::to_vec(&state).expect("encode provider transition"),
            )
            .expect("record provider transition while atomic is stopped");
        }
        let completed: Value = serde_json::from_slice(&success(resume(&root, None)))
            .expect("parse completed delivery");
        assert_eq!(completed["effects"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            completed["queue_observation"]["outcome"]["status"],
            if final_status == "removed" {
                "failed"
            } else {
                "pending"
            }
        );
        if final_status == "removed" {
            fs::remove_file(dir.join("unreadable-enqueue"))
                .expect("restore queue observation after delivery");
        }
        let observe = || -> Value {
            serde_json::from_slice(&success(cli(&root, &["delivery", "observe", RUN], None)))
                .expect("parse queue observation")
        };
        let read_state = || -> Value {
            serde_json::from_slice(&fs::read(dir.join("remote-state.json")).expect("fake state"))
                .expect("parse fake state")
        };
        if final_status == "merged" {
            assert_eq!(
                read_state()["observe_calls"]["enqueue"],
                1,
                "replay must use the checkpointed initial read after provider transition"
            );
            let rt = Durare::open(
                &Database::Sqlite(root.join("state/private").join(&id).join("atomic.db")),
                None,
                LAYOUT_VERSION,
                &format!("project/{id}"),
            )
            .await
            .expect("open resumed observation store");
            let stored: String = rt
                .control_transaction(|mut tx| {
                    Box::pin(async move {
                        tx.query_one(
                            "SELECT status FROM atomic_delivery_queue_observations WHERE delivery_run=?",
                            &[text(RUN)],
                        )
                        .await?
                        .text("status")
                    })
                })
                .await
                .expect("read initial checkpointed observation");
            assert_eq!(stored, "pending", "row must agree with replayed run output");
            drop(rt);
        }
        if final_status == "removed" {
            // The remote effect was applied while this run and source were valid.
            // Later local state changes must not prevent a read-only observation.
            let rt = Durare::open(
                &Database::Sqlite(root.join("state/private").join(&id).join("atomic.db")),
                None,
                LAYOUT_VERSION,
                &format!("project/{id}"),
            )
            .await
            .expect("open delivered store for historical observation");
            rt.control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.execute("UPDATE atomic_graph_items SET status='open' WHERE id='item'", &[])
                        .await?;
                    tx.execute(
                        "UPDATE workflow_status SET status='ERROR', error='later local failure' WHERE workflow_uuid=?",
                        &[text(RUN)],
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .expect("change local run and source after enqueue");
            drop(rt);
            fs::write(dir.join("unreadable-enqueue"), "").expect("block provider read");
            let failed = observe();
            assert_eq!(failed["outcome"]["status"], "failed");
            fs::remove_file(dir.join("unreadable-enqueue")).expect("restore provider read");
            let pending = observe();
            assert_eq!(pending["outcome"]["status"], "pending");
            assert_eq!(pending["head"], HEAD);
            assert_eq!(pending["pull_request_number"], 1);
            let mut config: Value = serde_json::from_slice(
                &fs::read(dir.join("target.json")).expect("read operator target"),
            )
            .expect("parse operator target");
            config["title"] = json!("Later change");
            config["repository"] = json!("test/later-repo");
            config["branch_ref"] = json!("refs/heads/later-lane");
            config["expected_old"] = json!(HEAD);
            fs::write(
                dir.join("target.json"),
                serde_json::to_vec(&config).expect("encode later target"),
            )
            .expect("write later operator target");
            assert_eq!(observe()["outcome"]["status"], "pending");
            fs::remove_file(dir.join("target.json")).expect("retire old operator target");
        }
        let mut state = read_state();
        let queue_key = state["queue"]
            .as_object()
            .expect("fake queue map")
            .keys()
            .next()
            .expect("queued pull request")
            .clone();
        if final_status == "removed" {
            state["queue"][&queue_key]["head"] = json!(TRIAL_HEAD);
            fs::write(
                dir.join("remote-state.json"),
                serde_json::to_vec(&state).expect("encode wrong provider head"),
            )
            .expect("write wrong provider head");
            assert_eq!(observe()["outcome"]["status"], "failed");
            state["queue"][&queue_key]["head"] = json!(HEAD);
        }
        state["queue"][&queue_key]["status"] = json!(final_status);
        state["queue"][&queue_key]["event_id"] = json!(format!("queue:{final_status}:1"));
        state["queue"][&queue_key]["happened_at"] = json!(1_800_000_000_000_i64);
        if final_status == "merged" {
            state["queue"][&queue_key]["merge_sha"] = json!(TRIAL_HEAD);
        }
        fs::write(
            dir.join("remote-state.json"),
            serde_json::to_vec(&state).expect("encode provider event"),
        )
        .expect("write provider event");
        let terminal = observe();
        assert_eq!(
            terminal["outcome"]["status"],
            final_status.replace("removed", "dropped")
        );
        assert_eq!(
            terminal["outcome"]["event_id"],
            format!("queue:{final_status}:1")
        );
        if final_status == "merged" {
            assert_eq!(terminal["outcome"]["merge_sha"], TRIAL_HEAD);
        }
        assert_eq!(observe(), terminal, "terminal event must be replay-stable");
        let final_state = read_state();
        assert_eq!(
            final_state["mutations"],
            json!({"push":1,"pull_request":1,"enqueue":1}),
            "observation must not dispatch another remote effect"
        );
        let rt = Durare::open(
            &Database::Sqlite(root.join("state/private").join(&id).join("atomic.db")),
            None,
            LAYOUT_VERSION,
            &format!("project/{id}"),
        )
        .await
        .expect("open observed store");
        let rows: i64 = rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx
                        .query_one(
                            "SELECT COUNT(*) AS n FROM atomic_delivery_queue_observations WHERE delivery_run=?",
                            &[text(RUN)],
                        )
                        .await?
                        .int("n")
                })
            })
            .await
            .expect("count durable observation rows");
        assert_eq!(rows, 1, "restart must not duplicate the outcome");
        drop(rt);
        fs::remove_dir_all(root).expect("remove observation fixture");
    }
}
