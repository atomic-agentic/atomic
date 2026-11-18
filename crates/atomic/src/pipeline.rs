// SPDX-License-Identifier: Apache-2.0

//! `run_pipeline`: one durable workflow that runs any workflow file.
//!
//! A workflow file is data. [`prepare`] turns a loaded and checked workflow,
//! and the atoms it names, into a [`Pipeline`] that carries them by value:
//! the steps in order, each `with` mapping and the `returns` expression as CEL
//! source, the contracts, and every atom with its sources and compiled
//! contracts. A run takes the pipeline as its input, so the durable record
//! holds exactly what the run started with, and nothing is rebuilt to run a
//! new workflow.
//!
//! The interpreter walks the steps in order. Each atom step evaluates its
//! mapping against the workflow's `input` and the published outputs of the
//! steps before it, then runs the atom through [`run_atom`]: its turn and its
//! publication are sibling durable steps, so a step that finished before a
//! crash is never run again. CEL has no side effects, so mappings evaluated
//! again on replay give the same values.
//!
//! v0.1 runs sequential atom steps. Gate steps and worktree steps are refused
//! by [`prepare`] until the interpreter supports them.

use crate::workflows::propose;
use atomic_core::atom::Atom;
use atomic_core::runtime::{Runtimes, run_declarative};
use atomic_core::workflow::Workflow;
use atomic_core::{AtomError, compile};
use atomic_durable::{DurableError, DurableRuntime, Result, WorkflowContext, failpoint, run_atom};
use atomic_schema::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The name `run_pipeline` is registered under.
pub const PIPELINE: &str = "atomic.pipeline";

/// One atom step, as the interpreter runs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtomStep {
    /// The step's id in the workflow.
    pub id: String,
    /// The atom it runs, by name.
    pub atom: String,
    /// Each input field and the CEL expression that computes it.
    pub with: BTreeMap<String, String>,
}

/// A workflow and the atoms it uses, by value: what one run executes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pipeline {
    /// The workflow's name.
    pub name: String,
    /// Its atom steps, in an order that respects `after`.
    pub steps: Vec<AtomStep>,
    /// The CEL expression for its result.
    pub returns: String,
    /// Its input contract.
    pub input: Value,
    /// Its output contract.
    pub output: Value,
    /// Every atom a step names, by name.
    pub atoms: BTreeMap<String, Atom>,
}

/// One run of a pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineRun {
    /// What to run.
    pub pipeline: Pipeline,
    /// The workflow's input.
    pub input: Value,
}

/// Why a workflow cannot run.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{workflow}: {message}")]
pub struct PrepareError {
    /// The workflow's name.
    pub workflow: String,
    /// What is wrong.
    pub message: String,
}

/// Turn a loaded, checked workflow and the project's atoms into a pipeline,
/// refusing what the interpreter cannot run yet.
pub fn prepare(
    workflow: &Workflow,
    atoms: &BTreeMap<String, Atom>,
) -> std::result::Result<Pipeline, PrepareError> {
    let fail = |message: String| PrepareError {
        workflow: workflow.name.clone(),
        message,
    };
    let definition = &workflow.definition;
    let mut steps = Vec::new();
    let mut used = BTreeMap::new();
    for id in &workflow.order {
        let step = &definition["steps"][id];
        if step.get("gate").is_some() {
            return Err(fail(format!(
                "step `{id}` is a gate; this version of run_pipeline runs atom steps only"
            )));
        }
        if step.get("worktree").and_then(Value::as_bool) == Some(true) {
            return Err(fail(format!(
                "step `{id}` asks for a worktree, which run_pipeline does not provide yet"
            )));
        }
        let name = step
            .get("atom")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(format!("step `{id}` names no atom")))?;
        let atom = atoms
            .get(name)
            .ok_or_else(|| fail(format!("step `{id}` names `{name}`, which is not loaded")))?;
        if !matches!(atom.kind.as_str(), "lite" | "codex") {
            return Err(fail(format!(
                "step `{id}` runs `{name}`, a {} atom; run_pipeline runs atoms that call a model",
                atom.kind
            )));
        }
        let mut with = BTreeMap::new();
        if let Some(map) = step.get("with").and_then(Value::as_object) {
            for (field, source) in map {
                let source = source.as_str().ok_or_else(|| {
                    fail(format!(
                        "step `{id}` maps `{field}` to a value that is not a CEL expression"
                    ))
                })?;
                with.insert(field.clone(), source.to_owned());
            }
        }
        used.insert(name.to_owned(), atom.clone());
        steps.push(AtomStep {
            id: id.clone(),
            atom: name.to_owned(),
            with,
        });
    }
    let returns = definition
        .get("returns")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("`returns` is not a CEL expression".into()))?
        .to_owned();
    Ok(Pipeline {
        name: workflow.name.clone(),
        steps,
        returns,
        input: workflow.input.clone(),
        output: workflow.output.clone(),
        atoms: used,
    })
}

fn failed(message: String) -> DurableError {
    DurableError::Failed(message)
}

/// Check `value` against `contract`, naming every place it fails.
fn fits(what: &str, contract: &Value, value: &Value) -> Result<()> {
    let validator = Registry::new()
        .validator_for(contract)
        .map_err(|e| failed(format!("{what}: the contract is not valid: {e}")))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/" } else { &at })
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(failed(format!(
            "{what} does not fit its contract: {}",
            errors.join("; ")
        )))
    }
}

/// Run one pipeline. Workflow code: everything that touches a model or the
/// store goes through durable steps.
async fn run_pipeline<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    runtimes: Arc<Runtimes>,
) -> Result<Value> {
    let PipelineRun { pipeline, input } = run;
    fits("the workflow's input", &pipeline.input, &input)?;
    let mut steps = Map::new();
    for step in &pipeline.steps {
        let scope = json!(steps);
        let values = [("input", &input), ("steps", &scope)];
        let mut mapped = Map::new();
        for (field, source) in &step.with {
            let value = atomic_schema::expr::evaluate(source, &values)
                .map_err(|e| failed(format!("step `{}`: `with.{field}`: {e}", step.id)))?;
            mapped.insert(field.clone(), value);
        }
        let atom = pipeline
            .atoms
            .get(&step.atom)
            .cloned()
            .ok_or_else(|| failed(format!("step `{}`: no atom `{}`", step.id, step.atom)))?;
        let runtimes = runtimes.clone();
        let run_id = ctx.workflow_id().to_owned();
        let atom_input = Value::Object(mapped);
        let published = run_atom(
            &ctx,
            &step.id,
            move || {
                propose(async move {
                    let mut registry = Registry::new();
                    let compiled = compile::compile(&atom, &registry)
                        .map_err(|e| AtomError::Runtime(e.to_string()))?;
                    // A workflow's input and every step's output are data the
                    // workflow's caller or a model supplied: untrusted.
                    run_declarative(
                        &atom,
                        &compiled,
                        &mut registry,
                        &atom_input,
                        &run_id,
                        "external",
                        &runtimes,
                    )
                    .await
                })
            },
            Vec::new(),
        )
        .await?;
        failpoint(&format!("pipeline:after:{}", step.id));
        steps.insert(
            step.id.clone(),
            json!({"output": published.output, "metadata": published.metadata}),
        );
    }
    let scope = json!(steps);
    let result =
        atomic_schema::expr::evaluate(&pipeline.returns, &[("input", &input), ("steps", &scope)])
            .map_err(|e| failed(format!("`returns`: {e}")))?;
    fits("the workflow's result", &pipeline.output, &result)?;
    Ok(result)
}

/// Register `run_pipeline` on `rt`, running atoms on `runtimes`.
pub fn register_pipeline<R: DurableRuntime>(rt: &mut R, runtimes: Arc<Runtimes>) {
    rt.register(PIPELINE, move |ctx: R::Context, run: PipelineRun| {
        run_pipeline(ctx, run, runtimes.clone())
    });
}
