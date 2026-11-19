// SPDX-License-Identifier: Apache-2.0

//! What `atomic run` runs: an atom or a workflow, named or given by its file,
//! with its input, as a [`PipelineRun`].
//!
//! A name is looked up in the project: `--project` (the manifest or its
//! directory) when given, else the project found by walking up from the
//! working directory. A path to an `.atom.astro` or `.workflow.yaml` file
//! runs that file, in the project found by walking up from it. Either way
//! the same definition gives the same run.
//!
//! A workflow runs as it is written. An atom runs as a workflow of one step,
//! `run`, that passes the whole input to the atom and returns its output.

use crate::failure::Failure;
use anyhow::Context;
use atomic_app::pipeline::{AtomStep, Pipeline, PipelineRun, Step, prepare};
use atomic_core::atom::{self, Atom};
use atomic_core::project::{self, Project};
use atomic_core::{collections, workflow};
use atomic_durable::gate::Deployment;
use atomic_schema::registry::Registry;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The suffix of an atom file.
const ATOM_SUFFIX: &str = ".atom.astro";
/// The suffix of a workflow file.
const WORKFLOW_SUFFIX: &str = ".workflow.yaml";

/// The project `--project` names: the manifest itself, or the directory
/// holding it (directly, or as `.atomic/atomic.yaml`).
pub fn named_project(path: &Path) -> anyhow::Result<Project> {
    if path.is_file() {
        return Ok(project::load(path)?);
    }
    if !path.is_dir() {
        return Err(Failure::new(
            "project.not_found",
            format!("--project {}: no such file or directory", path.display()),
            "pass the directory holding atomic.yaml (or .atomic/atomic.yaml), or the \
             manifest file itself",
        )
        .into());
    }
    let dedicated = path.join("atomic.yaml");
    let embedded = path.join(".atomic").join("atomic.yaml");
    match (dedicated.is_file(), embedded.is_file()) {
        (true, true) => Err(project::ProjectError::BothManifests(path.to_owned()).into()),
        (true, false) => Ok(project::load(&dedicated)?),
        (false, true) => Ok(project::load(&embedded)?),
        (false, false) => Err(Failure::new(
            "project.not_found",
            format!(
                "--project {}: the directory holds no atomic.yaml or .atomic/atomic.yaml",
                path.display()
            ),
            "pass the directory that holds the manifest, or the manifest file itself",
        )
        .into()),
    }
}

/// Whether `target` names a file rather than an atom or workflow.
pub fn is_file_target(target: &str) -> bool {
    target.ends_with(ATOM_SUFFIX)
        || target.ends_with(WORKFLOW_SUFFIX)
        || Path::new(target).is_file()
}

/// The project a command works on: `--project` when given; else, for a
/// file target, the project found by walking up from the file; else the
/// project found by walking up from `cwd`.
pub fn project_for(
    flag: Option<&Path>,
    target: Option<&str>,
    cwd: &Path,
) -> anyhow::Result<Project> {
    if let Some(flag) = flag {
        return named_project(&cwd.join(flag));
    }
    if let Some(target) = target.filter(|t| is_file_target(t)) {
        let file = file(target, cwd)?;
        let dir = file.parent().unwrap_or(Path::new("/"));
        return Ok(project::discover(dir)?);
    }
    Ok(project::discover(cwd)?)
}

fn file(target: &str, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = cwd.join(target);
    std::fs::canonicalize(&path).map_err(|e| {
        Failure::new(
            "target.not_found",
            format!("{}: {e}", path.display()),
            "give the path of an existing .atom.astro or .workflow.yaml file, or the name of \
             an atom or workflow in the project",
        )
        .into()
    })
}

/// The project's schemas and atoms, loaded and checked.
fn load_atoms(
    files: &BTreeMap<String, Vec<collections::SourceFile>>,
    registry: &mut Registry,
) -> anyhow::Result<BTreeMap<String, Atom>> {
    for schema in &files["schemas"] {
        registry.load_file(&schema.path)?;
    }
    let mut atoms = BTreeMap::new();
    for source in &files["atoms"] {
        let atom = atom::load(&source.path, registry)?;
        atoms.insert(atom.name.clone(), atom);
    }
    Ok(atoms)
}

/// What a name or file resolved to.
enum Found {
    Atom(PathBuf),
    Workflow(PathBuf),
}

fn find(
    project: &Project,
    files: &BTreeMap<String, Vec<collections::SourceFile>>,
    target: &str,
    cwd: &Path,
) -> anyhow::Result<Found> {
    if is_file_target(target) {
        let file = file(target, cwd)?;
        let name = file.to_string_lossy();
        if name.ends_with(ATOM_SUFFIX) {
            return Ok(Found::Atom(file));
        }
        if name.ends_with(WORKFLOW_SUFFIX) {
            return Ok(Found::Workflow(file));
        }
        return Err(Failure::new(
            "target.unknown",
            format!("{} is neither an atom nor a workflow file", file.display()),
            "give a file ending in .atom.astro or .workflow.yaml, or the name of an atom or \
             workflow",
        )
        .into());
    }
    let named = |collection: &str| {
        files[collection]
            .iter()
            .find(|f| f.name == target)
            .map(|f| f.path.clone())
    };
    match (named("atoms"), named("workflows")) {
        (Some(atom), None) => Ok(Found::Atom(atom)),
        (None, Some(workflow)) => Ok(Found::Workflow(workflow)),
        (Some(atom), Some(workflow)) => Err(Failure::new(
            "target.ambiguous",
            format!(
                "`{target}` names both the atom {} and the workflow {}",
                atom.display(),
                workflow.display()
            ),
            "give the path of the file to run, or rename one of the two",
        )
        .into()),
        (None, None) => {
            let mut names: Vec<&str> = files["atoms"]
                .iter()
                .chain(&files["workflows"])
                .map(|f| f.name.as_str())
                .collect();
            names.sort_unstable();
            Err(Failure::new(
                "target.unknown",
                format!(
                    "project `{}` ({}) has no atom or workflow named `{target}`",
                    project.name,
                    project.root().display()
                ),
                if names.is_empty() {
                    "add an atom or a workflow to the project, or give the path of one".to_owned()
                } else {
                    format!(
                        "name one of {}, or give the path of an .atom.astro or .workflow.yaml \
                         file",
                        names.join(", ")
                    )
                },
            )
            .into())
        }
    }
}

/// The run `atomic run target` starts on `input`, in `project`.
pub fn load(
    project: &Project,
    target: &str,
    cwd: &Path,
    input: Value,
    deployment: &Deployment,
) -> anyhow::Result<PipelineRun> {
    let files = collections::collect(project)?;
    let mut registry = Registry::new();
    let pipeline = match find(project, &files, target, cwd)? {
        Found::Atom(file) => {
            for schema in &files["schemas"] {
                registry.load_file(&schema.path)?;
            }
            let atom = atom::load(&file, &mut registry)?;
            atom_pipeline(atom, &input)?
        }
        Found::Workflow(file) => {
            let atoms = load_atoms(&files, &mut registry)?;
            let loaded = workflow::load(&file, &atoms, &mut registry)?;
            prepare(&loaded, &atoms, deployment)?
        }
    };
    refuse_writes_outside_a_worktree(&pipeline)?;
    check_input(&pipeline, &input)?;
    Ok(PipelineRun { pipeline, input })
}

/// An atom as a workflow of one step, `run`, that passes the whole input to
/// it and returns its output.
fn atom_pipeline(atom: Atom, input: &Value) -> anyhow::Result<Pipeline> {
    let contract = |key: &str| {
        atom.program
            .contracts
            .get(key)
            .cloned()
            .with_context(|| format!("atom `{}` has no compiled {key} contract", atom.name))
    };
    let (input_contract, output_contract) = (contract("Input")?, contract("Output")?);
    let Some(fields) = input.as_object() else {
        return Err(Failure::new(
            "input.invalid",
            format!("the input to atom `{}` is not a JSON object", atom.name),
            "pass a JSON object with the atom's input fields",
        )
        .into());
    };
    // Each field present, mapped by name: an optional field left out stays
    // out.
    let with: BTreeMap<String, String> = fields
        .keys()
        .map(|k| (k.clone(), format!("input[{}]", Value::String(k.clone()))))
        .collect();
    let pins = atom
        .program
        .sources
        .iter()
        .map(|(source, text)| (source.clone(), sha256(text.as_bytes())))
        .collect();
    Ok(Pipeline {
        name: atom.name.clone(),
        steps: vec![Step::Atom(AtomStep {
            id: "run".into(),
            atom: atom.name.clone(),
            origins: with
                .keys()
                .map(|k| (k.clone(), "external".to_owned()))
                .collect(),
            with,
            worktree: false,
        })],
        returns: "steps.run.output".into(),
        input: input_contract,
        output: output_contract,
        atoms: BTreeMap::from([(atom.name.clone(), atom)]),
        pins,
        // A single atom's own budget block applies through the atom; the
        // run itself has the workflow defaults.
        budget: Default::default(),
    })
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// A Codex atom that may write runs only in a worktree of its own: never in
/// the developer's checkout.
fn refuse_writes_outside_a_worktree(pipeline: &Pipeline) -> anyhow::Result<()> {
    for step in &pipeline.steps {
        let Step::Atom(step) = step else { continue };
        let Some(atom) = pipeline.atoms.get(&step.atom) else {
            continue;
        };
        let writes =
            atom.frontmatter.get("sandbox").and_then(Value::as_str) == Some("workspace-write");
        if atom.kind == "codex" && writes && !step.worktree {
            return Err(Failure::new(
                "target.unsafe",
                format!(
                    "step `{}` runs atom `{}`, which may write files, outside a worktree, so \
                     it would write in the project's own checkout",
                    step.id, step.atom
                ),
                "run it from a workflow step with `worktree: true` and a `base` commit, or \
                 make the atom read-only",
            )
            .into());
        }
    }
    Ok(())
}

/// Check the input against the run's contract before anything is recorded.
fn check_input(pipeline: &Pipeline, input: &Value) -> anyhow::Result<()> {
    let validator = Registry::new()
        .validator_for(&pipeline.input)
        .with_context(|| format!("the input contract of `{}`", pipeline.name))?;
    let errors: Vec<String> = validator
        .iter_errors(input)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/" } else { &at })
        })
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    Err(Failure::new(
        "input.invalid",
        format!(
            "the input does not fit the input contract of `{}`: {}",
            pipeline.name,
            errors.join("; ")
        ),
        "change the input where each error points; nothing was started",
    )
    .into())
}

/// Whether any step of `run` calls a model.
pub fn needs_model(run: &PipelineRun) -> bool {
    run.pipeline
        .atoms
        .values()
        .any(|a| matches!(a.kind.as_str(), "lite" | "codex"))
}

/// The run's input: the file `--input` names (`-` for stdin), or `--data`.
pub fn input(file: Option<&Path>, data: Option<&str>, cwd: &Path) -> anyhow::Result<Value> {
    let (text, from) = match (file, data) {
        (Some(_), Some(_)) => {
            return Err(Failure::new(
                "input.ambiguous",
                "both --input and --data were given",
                "pass the input once: --input <file>, --input - for stdin, or --data '<json>'",
            )
            .into());
        }
        (None, None) => {
            return Err(Failure::new(
                "input.missing",
                "no input was given",
                "pass --input <file>, --input - to read standard input, or --data '<json>'",
            )
            .into());
        }
        (None, Some(data)) => (data.to_owned(), "--data".to_owned()),
        (Some(path), None) if path == Path::new("-") => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("read the input from standard input")?;
            (text, "standard input".to_owned())
        }
        (Some(path), None) => {
            let path = cwd.join(path);
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Failure::new(
                    "input.unreadable",
                    format!("--input {}: {e}", path.display()),
                    "give the path of a readable JSON file, or - for standard input",
                )
            })?;
            (text, path.display().to_string())
        }
    };
    serde_json::from_str(&text).map_err(|e| {
        Failure::new(
            "input.not_json",
            format!("the input from {from} is not one JSON value: {e}"),
            "pass a single JSON value, such as an object with the input's fields",
        )
        .into()
    })
}
