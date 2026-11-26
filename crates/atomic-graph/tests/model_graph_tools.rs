// SPDX-License-Identifier: Apache-2.0

//! Model-facing graph tool regressions.

use atomic_core::codex::NativeToolHost;
use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
use atomic_core::testing::{FakeModel, Reply, denying_nothing, on_codex_runtime};
use atomic_core::typed::AtomConfig;
use atomic_core::usage::Metering;
use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::publication::{Proposal, publish_candidate_in, review_candidate};
use atomic_durable::release::{ReleaseCommand, release_park};
use atomic_durable::{
    DurableError, DurableRuntime, Durare, DurareContext, WorkflowContext, execute_candidate_told,
    parks,
};
use atomic_graph::proposal::{AttemptKey, GraphOp, ProposalBuffer};
use atomic_graph::publication::apply_candidate_batch;
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::Lease;
use atomic_graph::store::{Actor, Draft, EdgeKind, GraphStore};
use atomic_graph::tools::GraphTurnTools;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn model_graph_create_is_private_until_candidate_publication() {
    let path = std::env::temp_dir().join(format!("atomic-graph-tool-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(SqliteStore::open(&path, SystemTime::now).expect("graph store"));
    let buffer =
        ProposalBuffer::new(AttemptKey::new("run", "step", 1).expect("graph tool regression"));
    let tools = GraphTurnTools::new(Arc::clone(&store), buffer);

    let receipt = tools
        .call(
            "graph_create",
            json!({"title":"Draft only","body":"Candidate text","kind":"task","priority":1}),
        )
        .await
        .expect("tool call");
    let id = receipt["provisional_id"].as_str().expect("provisional id");
    assert_eq!(receipt["committed"], false);
    assert!(
        store
            .show(Actor::agent("test"), id.to_owned())
            .await
            .expect("graph tool regression")
            .is_none()
    );
    assert_eq!(
        tools
            .call("graph_ready", json!({"limit":10}))
            .await
            .expect("graph tool regression"),
        json!([])
    );
    tools
        .call(
            "graph_remember",
            json!({"item_id":id,"text":"Private note"}),
        )
        .await
        .expect("graph tool regression");
    tools
        .call(
            "graph_dep_add",
            json!({"from":id,"kind":"related","to":"at-earlier"}),
        )
        .await
        .expect("graph tool regression");
    tools
        .call(
            "graph_close",
            json!({"item_id":id,"reason":"Proposed only"}),
        )
        .await
        .expect("graph tool regression");
    let view = tools
        .call("graph_show", json!({"item_id":id}))
        .await
        .expect("graph tool regression");
    assert_eq!(view["provisional"], true);
    assert_eq!(view["status"], "open");
    assert_eq!(view["notes"][0]["text"], "Private note");
    assert_eq!(view["notes"][0]["trust"], "external");
    assert_eq!(view["edges"][0]["to"], "at-earlier");
    assert_eq!(view["close_intent"]["text"], "Proposed only");
    assert!(
        tools
            .call(
                "graph_create",
                json!({"title":"bad","body":"","kind":"task",
        "priority":1,"actor":"human"})
            )
            .await
            .is_err()
    );
    let batch = tools.operations().await;
    assert_eq!(batch.len(), 4);
    assert!(matches!(&batch[0].op, GraphOp::Create { title, .. } if title == "Draft only"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn graph_show_includes_committed_edges_and_current_blockers() {
    let path = std::env::temp_dir().join(format!("atomic-graph-show-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(SqliteStore::open(&path, SystemTime::now).expect("graph store"));
    for id in ["at-source", "at-blocker"] {
        let mut draft = Draft::new(id, id);
        if id == "at-source" {
            draft.due_at = Some(UNIX_EPOCH - Duration::from_secs(1));
        }
        store
            .create_item(Actor::agent("test"), format!("create-{id}"), draft)
            .await
            .expect("create committed item");
    }
    store
        .add_edge(
            Actor::agent("test"),
            "at-source".into(),
            EdgeKind::Blocks,
            "at-blocker".into(),
        )
        .await
        .expect("create committed edge");
    let tools = GraphTurnTools::new(
        Arc::clone(&store),
        ProposalBuffer::new(AttemptKey::new("run", "step", 1).expect("attempt")),
    );
    let view = tools
        .call("graph_show", json!({"item_id":"at-source"}))
        .await
        .expect("show committed item");
    assert_eq!(
        view["edges"],
        json!([{"kind":"blocks","to":"at-blocker","provisional":false}])
    );
    assert_eq!(view["blocked_by"], json!(["at-blocker"]));
    assert_eq!(view["provisional"], false);
    assert_eq!(view["trust"], "agent");
    assert_eq!(view["due_at"], -1000);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn parked_graph_author_uses_first_turn_identity_when_later_candidate_publishes() {
    on_codex_runtime(async {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "atomic-graph-park-tool-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("home")).expect("model home");
        std::fs::create_dir_all(root.join("cwd")).expect("model cwd");
        let database = root.join("graph.db");
        let mut runtime = Durare::sqlite(&database).await.expect("durable database");
        let store = Arc::new(SqliteStore::open(&database, SystemTime::now).expect("graph store"));
        store
            .create_item(
                Actor::agent("fixture"),
                "create-root".into(),
                Draft::new("at-root", "Root"),
            )
            .await
            .expect("root item");
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "graph_create".into(),
                arguments: json!({
                    "title":"After park", "body":"first attempt identity",
                    "kind":"task", "priority":2,
                }),
            },
            Reply::Text("Done.".into()),
        ])
        .await
        .expect("scripted model");
        let codex = CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: root.join("home"),
            guard: StateGuard::NoProjectState,
            cwd: root.join("cwd"),
            endpoint: ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            },
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: Metering::none(),
            caches: None,
        };
        let turns = Arc::new(AtomicUsize::new(0));
        let scoped_store = Arc::clone(&store);
        runtime.register("parked-graph-author", move |ctx: DurareContext, (): ()| {
            let turns = Arc::clone(&turns);
            let store = Arc::clone(&scoped_store);
            let codex = codex.clone();
            async move {
                let (candidate, first_position) =
                    execute_candidate_told(&ctx, "author.turn", move |info| {
                        let turns = Arc::clone(&turns);
                        let store = Arc::clone(&store);
                        let codex = codex.clone();
                        async move {
                            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                                return Err(DurableError::Parked {
                                    reason: "wait for operator".into(),
                                    scope: None,
                                });
                            }
                            let attempt = AttemptKey::new(
                                &info.workflow_id,
                                "author",
                                u32::try_from(info.step_id).expect("nonnegative author position")
                                    + 1,
                            )
                            .map_err(|error| DurableError::Failed(error.to_string()))?;
                            let tools =
                                Arc::new(GraphTurnTools::new(store, ProposalBuffer::new(attempt)));
                            let spec = TurnSpec {
                                prompt: "Create a follow-up after release.".into(),
                                native_tools: Some(tools.clone()),
                                ..TurnSpec::default()
                            };
                            let answer =
                                run_turn(&codex, &denying_nothing(Default::default()), &spec)
                                    .await
                                    .map_err(|error| DurableError::Failed(error.to_string()))?;
                            Ok(Proposal {
                                output: json!(answer),
                                metadata: json!({
                                    "graph_attempt_step":"author",
                                    "graph_attempt_position":info.step_id,
                                    "graph_operations":tools.operations().await,
                                }),
                                writes: Vec::new(),
                            })
                        }
                    })
                    .await?;
                let current_position: i32 = candidate
                    .id
                    .rsplit_once(':')
                    .and_then(|(_, position)| position.parse().ok())
                    .expect("candidate position");
                assert!(
                    current_position > first_position,
                    "author did not retry after park"
                );
                let candidate = Arc::new(candidate);
                ctx.transaction("author.publish", move |mut tx| {
                    let candidate = Arc::clone(&candidate);
                    Box::pin(async move {
                        publish_candidate_in(
                            &mut tx,
                            "author.publish",
                            &candidate,
                            &review_candidate(&candidate),
                            &[],
                        )
                        .await?;
                        apply_candidate_batch(
                            &mut tx,
                            &candidate,
                            &Lease {
                                item_id: "at-root".into(),
                                claimant: "worker".into(),
                                session: 1,
                                epoch: 1,
                                acquisition: "park-claim".into(),
                            },
                        )
                        .await
                    })
                })
                .await?;
                Ok(first_position)
            }
        });
        let credential = HumanCredential::new(Principal::local("operator"), "graph-park-secret");
        gate::enroll(&runtime, &credential)
            .await
            .expect("enroll operator");
        runtime.launch().await.expect("launch runtime");
        let run = "graph-park-author";
        runtime
            .start("parked-graph-author", run, ())
            .await
            .expect("start graph author");
        let park = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(park) = parks(&runtime, run)
                    .await
                    .expect("read parks")
                    .first()
                    .cloned()
                {
                    break park;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("author park");
        release_park(
            &runtime,
            &credential,
            ReleaseCommand {
                command_id: "release-graph-author".into(),
                park: park.id,
                raise: None,
                comment: None,
            },
        )
        .await
        .expect("release parked author");
        let _: i32 = runtime
            .result(run)
            .await
            .expect("published graph candidate");
        assert_eq!(model.calls(), 2, "park repeated a model call");
        assert_eq!(
            store
                .ready(Actor::agent("reader"), 10)
                .await
                .expect("committed items")
                .iter()
                .filter(|item| item.title == "After park")
                .count(),
            1,
        );
        runtime
            .shutdown(Duration::from_secs(5))
            .await
            .expect("shutdown runtime");
        let _ = std::fs::remove_dir_all(root);
    });
}

#[test]
fn codex_model_call_reaches_private_graph_buffer() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "graph_create".into(),
                arguments: json!({"title":"From model","body":"Candidate only",
                    "kind":"task","priority":1}),
            },
            Reply::Text("Done.".into()),
        ])
        .await
        .expect("fake model");
        let root = std::env::temp_dir().join(format!("atomic-graph-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).expect("model home");
        std::fs::create_dir_all(root.join("cwd")).expect("model cwd");
        let store = Arc::new(
            SqliteStore::open(&root.join("graph.db"), SystemTime::now).expect("graph store"),
        );
        let tools = Arc::new(GraphTurnTools::new(
            Arc::clone(&store),
            ProposalBuffer::new(AttemptKey::new("run", "step", 1).expect("attempt")),
        ));
        let runtime = CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: root.join("home"),
            guard: StateGuard::NoProjectState,
            cwd: root.join("cwd"),
            endpoint: ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            },
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: Metering::none(),
            caches: None,
        };
        let spec = TurnSpec {
            prompt: "Create a graph item.".into(),
            native_tools: Some(tools.clone()),
            ..TurnSpec::default()
        };
        let answer = run_turn(&runtime, &denying_nothing(Default::default()), &spec)
            .await
            .expect("Codex turn");
        assert_eq!(answer, "Done.");
        assert_eq!(model.calls(), 2);
        assert!(model.requests()[0].to_string().contains("graph_create"));
        let tool_result = model.requests()[1]["input"]
            .as_array()
            .expect("second model input")
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("graph tool result")["output"]
            .to_string();
        assert!(tool_result.contains("committed"), "{tool_result}");
        assert!(tool_result.contains("false"), "{tool_result}");
        let batch = tools.operations().await;
        assert_eq!(batch.len(), 1);
        let id = batch[0]
            .receipt
            .provisional_id
            .as_ref()
            .expect("provisional id");
        assert!(
            store
                .show(Actor::agent("test"), id.clone())
                .await
                .expect("committed graph read")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
    });
}
