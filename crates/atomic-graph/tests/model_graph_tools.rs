// SPDX-License-Identifier: Apache-2.0

//! Model-facing graph tool regressions.

use atomic_core::codex::NativeToolHost;
use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
use atomic_core::testing::{FakeModel, Reply, denying_nothing, on_codex_runtime};
use atomic_core::typed::AtomConfig;
use atomic_core::usage::Metering;
use atomic_graph::proposal::{AttemptKey, GraphOp, ProposalBuffer};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, GraphStore};
use atomic_graph::tools::GraphTurnTools;
use serde_json::json;
use std::sync::Arc;
use std::time::SystemTime;

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
