// SPDX-License-Identifier: Apache-2.0

//! Host-owned graph tools for one model attempt. Writes stay in the private
//! proposal buffer; this module has no committed write path.

use atomic_core::codex::{NativeToolHost, NativeToolSpec};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;

use crate::proposal::{
    AttemptKey, GraphView, ProposalBuffer, RecordedOp, ViewNote, ViewTrust, operation_schema,
};
use crate::sqlite::SqliteStore;
use crate::store::{Actor, GraphStore, Show, Trust};

/// A single attempt's graph capability. Clone shares the same append log.
pub struct GraphTurnTools {
    store: Arc<SqliteStore>,
    buffer: Arc<Mutex<ProposalBuffer>>,
    attempt: AttemptKey,
}

impl std::fmt::Debug for GraphTurnTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphTurnTools")
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl GraphTurnTools {
    /// Attach the private buffer and read-only committed view for one attempt.
    pub fn new(store: Arc<SqliteStore>, buffer: ProposalBuffer) -> Self {
        let attempt = buffer.attempt().clone();
        Self {
            store,
            buffer: Arc::new(Mutex::new(buffer)),
            attempt,
        }
    }

    /// Snapshot validated operations for the durable candidate checkpoint.
    pub async fn operations(&self) -> Vec<RecordedOp> {
        self.buffer.lock().await.operations().to_vec()
    }

    async fn invoke(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "graph_create" | "graph_close" | "graph_dep_add" | "graph_remember" => {
                let op = match name {
                    "graph_create" => "create",
                    "graph_close" => "close_intent",
                    "graph_dep_add" => "dep_add",
                    "graph_remember" => "remember",
                    _ => unreachable!(),
                };
                let mut object = arguments
                    .as_object()
                    .cloned()
                    .ok_or_else(|| "graph tool argument must be an object".to_owned())?;
                if object.contains_key("op") {
                    return Err("op is assigned by the host".into());
                }
                object.insert("op".into(), Value::String(op.into()));
                let receipt = self
                    .buffer
                    .lock()
                    .await
                    .propose_raw(Value::Object(object))
                    .map_err(|e| e.to_string())?;
                Ok(
                    json!({"index":receipt.index,"request_id":receipt.request_id,
                    "provisional_id":receipt.provisional_id,"committed":false}),
                )
            }
            "graph_show" => {
                let id = bounded_id(&arguments, "item_id")?;
                let committed = self
                    .store
                    .show(Actor::agent("graph-tool"), id.clone())
                    .await
                    .map_err(|e| e.to_string())?
                    .map(committed_view);
                let view = self.buffer.lock().await.show(&self.attempt, &id, committed);
                serde_json::to_value(view).map_err(|e| e.to_string())
            }
            "graph_ready" => {
                let object = arguments
                    .as_object()
                    .ok_or("graph tool argument must be an object")?;
                if object.keys().any(|key| key != "limit") {
                    return Err("unknown graph_ready argument".into());
                }
                let limit = object
                    .get("limit")
                    .and_then(Value::as_u64)
                    .ok_or("limit must be an integer")?;
                if !(1..=100).contains(&limit) {
                    return Err("limit must be between 1 and 100".into());
                }
                let items = self
                    .store
                    .ready(Actor::agent("graph-tool"), limit as usize)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Value::Array(
                    items
                        .into_iter()
                        .map(|item| {
                            json!({
                                "id":item.id,"title":item.title,"kind":item.kind,
                                "priority":item.priority,"status":item.status.as_str()
                            })
                        })
                        .collect(),
                ))
            }
            _ => Err("unknown graph tool".into()),
        }
    }
}

fn bounded_id(arguments: &Value, key: &str) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or("graph tool argument must be an object")?;
    if object.len() != 1 {
        return Err("unknown graph_show argument".into());
    }
    let id = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or("item_id must be a string")?;
    if id.is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
        return Err("invalid item_id".into());
    }
    Ok(id.to_owned())
}

fn committed_view(show: Show) -> GraphView {
    let item = show.item;
    GraphView {
        id: item.id,
        title: item.title,
        body: item.body,
        author: item.created_by,
        trust: view_trust(item.trust),
        kind: item.kind,
        priority: item.priority,
        status: item.status.as_str().into(),
        parent: item.parent,
        due_at: item
            .due_at
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
        provisional: false,
        close_intent: None,
        notes: show
            .notes
            .into_iter()
            .map(|note| ViewNote {
                text: note.text,
                author: note.author,
                trust: view_trust(note.trust),
                provisional: false,
            })
            .collect(),
        edges: Vec::new(),
    }
}

fn view_trust(trust: Trust) -> ViewTrust {
    trust.into()
}

fn proposal_schema(index: usize) -> Value {
    let mut schema = operation_schema()["oneOf"][index].clone();
    schema["properties"]
        .as_object_mut()
        .expect("static schema")
        .remove("op");
    schema["required"]
        .as_array_mut()
        .expect("static schema")
        .retain(|value| value != "op");
    schema
}

impl NativeToolHost for GraphTurnTools {
    fn specs(&self) -> Vec<NativeToolSpec> {
        vec![
            NativeToolSpec {
                name: "graph_ready",
                description: "List committed ready graph items.",
                parameters: json!({"type":"object","additionalProperties":false,
                    "required":["limit"],"properties":{"limit":{"type":"integer","minimum":1,"maximum":100}}}),
                external_context: true,
            },
            NativeToolSpec {
                name: "graph_show",
                description: "Show an item with this attempt's provisional graph proposals overlaid.",
                parameters: json!({"type":"object","additionalProperties":false,
                    "required":["item_id"],"properties":{"item_id":{"type":"string","minLength":1,"maxLength":160}}}),
                external_context: true,
            },
            NativeToolSpec {
                name: "graph_create",
                description: "Propose an item in this attempt's private candidate batch; it is not committed.",
                parameters: proposal_schema(0),
                external_context: false,
            },
            NativeToolSpec {
                name: "graph_close",
                description: "Propose a close intent; it is not committed.",
                parameters: proposal_schema(1),
                external_context: false,
            },
            NativeToolSpec {
                name: "graph_dep_add",
                description: "Propose a graph edge; it is not committed.",
                parameters: proposal_schema(2),
                external_context: false,
            },
            NativeToolSpec {
                name: "graph_remember",
                description: "Propose a note; it is not committed.",
                parameters: proposal_schema(3),
                external_context: false,
            },
        ]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(self.invoke(name, arguments))
    }
}
