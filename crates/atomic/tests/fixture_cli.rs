// SPDX-License-Identifier: Apache-2.0

//! A coding run through the `atomic` executable, bound to a fixture
//! repository: its worktree step runs a Codex atom under the real
//! `workspace-write` sandbox at the top of a worktree of the fixture's
//! snapshot, wherever the project is (outside any repository, or in a
//! subdirectory of the developer's), and atomic commits there. The
//! developer's checkout and the fixture repository itself are left exactly
//! as they were; nothing the fixture's configuration names (hooks, a file
//! system monitor, filter drivers, remotes) ever runs; and a run killed
//! after one step goes on from the snapshot after its source is gone, each
//! step's commit on the last.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

use git_repo::{git_in, git_state};

mod git_repo;
mod support;

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `--json` output, read back as JSON.
fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project with one codex atom that may write, run by `coding`, one
/// worktree step at the mapped base, `twice`, a second step on the
/// first's commit, and `relay`, one step at a base its input gives as a
/// plain string.
fn project(at: &Path) -> PathBuf {
    let project = at.to_owned();
    write(&project.join("atomic.yaml"), "atomic: 1\nname: coding\n");
    write(
        &project.join("atoms/fix-code.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/fix-code';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Fix the code.', \
         kind: 'codex', sandbox: 'workspace-write' };\n---\nFix this: {input.task}\n",
    );
    write(
        &project.join("atoms/fix-code.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["base", "task"],
            "properties": {"base": {"type": "string", "format": "git-commit"}, "task": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("atoms/fix-code.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["done"],
            "properties": {"done": {"type": "boolean"}}})
        .to_string(),
    );
    write(
        &project.join("workflows/coding.workflow.yaml"),
        "atomic: 1\nname: coding\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         returns: steps.fix.output\n",
    );
    write(
        &project.join("workflows/twice.workflow.yaml"),
        "atomic: 1\nname: twice\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  first:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         \x20 second:\n    after: [first]\n    atom: fix-code\n    \
         with: { base: steps.first.metadata.commit, task: input.task }\n    worktree: true\n\
         returns: steps.second.output\n",
    );
    write(
        &project.join("schemas/relay.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["from", "task"],
            "properties": {"from": {"type": "string"}, "task": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("workflows/relay.workflow.yaml"),
        "atomic: 1\nname: relay\n\
         input: { $ref: ../schemas/relay.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.from, task: input.task }\n    worktree: true\n\
         returns: steps.fix.output\n",
    );
    project
}

/// A fixture repository at `at` with one commit; returns it.
fn fixture(at: &Path) -> String {
    write(&at.join("price.txt"), "125\n");
    git_in(at, &["init", "-q"]);
    git_in(at, &["add", "price.txt"]);
    git_in(at, &["commit", "-q", "-m", "seed"]);
    git_in(at, &["rev-parse", "HEAD"])
}

/// A scratch directory of the test's own.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-fixture-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A directory of the test's own outside every git repository: under the
/// system's temporary directory, since the target directory is inside the
/// checkout being tested. Checked, so the test fails rather than run inside
/// a repository if that ever changes.
fn outside_any_repository(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-fixture-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "{} is inside a git work tree: {}",
        dir.display(),
        text(&inside)
    );
    dir
}

/// `atomic` with `args`, from `cwd`, on the fake model at `url`.
async fn atomic(dir: &Path, cwd: &Path, args: &[&str], url: &str, env: &[(&str, &str)]) -> Output {
    let (d, cwd, url) = (dir.to_owned(), cwd.to_owned(), url.to_owned());
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(ATOMIC);
        command
            .args(&args)
            .current_dir(&cwd)
            .env("HOME", d.join("home"))
            .env("XDG_CONFIG_HOME", d.join("home/.config"))
            .env("ATOMIC_HOME", d.join("atomic-home"))
            .env("USER", "dev")
            .env("ATOMIC_MODEL_BASE_URL", url)
            .env("ATOMIC_MODEL", "synthetic")
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("ATOMIC_DATABASE_URL")
            .env_remove("ATOMIC_FAILPOINT")
            .env_remove("CODEX_HOME");
        for (k, v) in &env {
            command.env(k, v);
        }
        command.output().unwrap()
    })
    .await
    .unwrap()
}

/// The snapshots atomic made under `dir`'s atomic home.
fn snapshots(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("atomic-home/projects"))
        .unwrap()
        .flat_map(|p| {
            std::fs::read_dir(p.unwrap().path().join("fixtures"))
                .into_iter()
                .flatten()
        })
        .map(|e| e.unwrap().path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect()
}

/// The commit atomic accepted for `step` of `run` in `snapshot`.
fn accepted(snapshot: &Path, run: &str, step: &str) -> String {
    let refs = git_in(
        snapshot,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("refs/atomic/{run}/"),
        ],
    );
    refs.lines()
        .find(|l| l.starts_with(&format!("refs/atomic/{run}/steps/{step}/")))
        .unwrap_or_else(|| panic!("no accepted commit for {step} in {refs}"))
        .split(' ')
        .nth(1)
        .unwrap()
        .to_owned()
}

/// What the model's command printed, in the request after its call.
fn printed(model: &FakeModel) -> String {
    model.requests()[1]["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `coding` bound to `fixture` at `main` from `project`, its turn
/// reading the fixture's file, and check it works at the top of the
/// snapshot's worktree and leaves both repositories as they were.
async fn coding_run_at_the_top(dir: &Path, project: &Path, fixture: &Path, base: &str) {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": "cat price.txt; pwd", "yield_time_ms": 20000}),
        },
        Reply::Text(r#"{"done": true}"#.into()),
    ])
    .await
    .unwrap();
    let source = git_state(fixture);
    let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
    let fixture_arg = fixture.to_string_lossy().into_owned();
    let out = atomic(
        dir,
        project,
        &[
            "--json",
            "run",
            "coding",
            "--run-id",
            "fix-1",
            "--data",
            &data,
            "--fixture",
            &fixture_arg,
            "--fixture-rev",
            "main",
        ],
        &model.base_url(),
        &[],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["output"], json!({"done": true}), "{doc}");
    // The turn worked at the top of its worktree, where the fixture's
    // files are, inside atomic's state.
    let said = printed(&model);
    assert!(said.contains("125"), "{said}");
    assert!(
        said.contains(&dir.join("atomic-home").display().to_string()),
        "{said}"
    );
    let snapshots = snapshots(dir);
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert!(snapshots[0].ends_with(format!("{base}.git")));
    let step = accepted(&snapshots[0], "fix-1", "fix");
    assert_eq!(
        git_in(&snapshots[0], &["rev-parse", &format!("{step}^")]),
        base
    );
    assert_eq!(git_state(fixture), source, "the fixture is as it was");
}

/// A fixture-bound coding step whose reviewer asks for changes, then
/// approves the repair: the repair round's worktree starts from the run's
/// base in the snapshot, as check_base requires, and the commit published is
/// the repair's, the one the second verdict was given.
#[test]
fn a_repair_round_of_a_fixture_bound_step_publishes_the_repairs_commit() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("repaired");
        let outside = outside_any_repository("repaired-project");
        let project = project(&outside.join("project"));
        reviewed_coding(&project);
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "request_changes"}"#.into()),
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "approve"}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
        let fixture_arg = dir.join("fixture").to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "reviewed",
                "--run-id",
                "repaired-1",
                "--data",
                &data,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 4);
        // The run keeps one accepted commit, the repair round's, made from
        // the run's base in the snapshot.
        let snapshots = snapshots(&dir);
        let refs = git_in(
            &snapshots[0],
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/atomic/repaired-1/",
            ],
        );
        let kept: Vec<&str> = refs.lines().collect();
        assert_eq!(kept.len(), 1, "{refs}");
        assert!(kept[0].contains("repair_1"), "{refs}");
        let repaired = kept[0].split(' ').nth(1).unwrap().to_owned();
        assert_eq!(
            git_in(&snapshots[0], &["rev-parse", &format!("{repaired}^")]),
            base
        );
        // The second verdict, which approved, was given the repair's
        // commit; the first, which asked for changes, another.
        let requests = model.requests();
        assert!(
            !requests[1].to_string().contains(&repaired),
            "{}",
            requests[1]
        );
        assert!(
            requests[3].to_string().contains(&repaired),
            "the approved candidate is not the repair's"
        );
        let _ = std::fs::remove_dir_all(&outside);
    });
}

/// The `reviewed` workflow and its reviewer, `check-commit`, in `project`.
fn reviewed_coding(project: &Path) {
    write(
        &project.join("atoms/check-commit.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/check-commit';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Check the commit.', \
         kind: 'lite' };\n---\nReview commit {input.commit}.\n",
    );
    write(
        &project.join("atoms/check-commit.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["commit"],
            "properties": {"commit": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("atoms/check-commit.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["verdict"],
            "properties": {"verdict": {"type": "string", "enum": ["approve", "request_changes"]}}})
        .to_string(),
    );
    write(
        &project.join("workflows/reviewed.workflow.yaml"),
        "atomic: 1\nname: reviewed\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         \x20   review:\n      reviewer: check-commit\n      with: { commit: candidate.metadata.commit }\n\
         returns: steps.fix.output\n",
    );
}

/// A fixture-bound coding step under review: its reviewer is given the
/// commit atomic made in the fixture's snapshot, as a SHA (it runs in the
/// project, not the snapshot, so the SHA is what it has of the change).
#[test]
fn a_reviewer_of_a_fixture_bound_step_is_given_the_commit_in_the_snapshot() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("reviewed");
        let outside = outside_any_repository("reviewed-project");
        let project = project(&outside.join("project"));
        reviewed_coding(&project);
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "approve"}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
        let fixture_arg = dir.join("fixture").to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "reviewed",
                "--run-id",
                "reviewed-1",
                "--data",
                &data,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let snapshots = snapshots(&dir);
        let step = accepted(&snapshots[0], "reviewed-1", "fix");
        let review = model.requests()[1].to_string();
        assert!(
            review.contains(&step),
            "the reviewer was not given {step}: {review}"
        );
        let _ = std::fs::remove_dir_all(&outside);
    });
}

#[test]
fn a_project_outside_any_repository_runs_its_coding_step_in_the_snapshot() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("outside");
        let outside = outside_any_repository("outside-project");
        let project = project(&outside.join("project"));
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        let _ = std::fs::remove_dir_all(&outside);
    });
}

#[test]
fn a_project_in_a_subdirectory_of_a_checkout_runs_at_the_top_of_the_fixture() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("subdirectory");
        let checkout = dir.join("dev");
        let project = project(&checkout.join("examples/coding"));
        git_in(&checkout, &["init", "-q"]);
        git_in(&checkout, &["add", "."]);
        git_in(&checkout, &["commit", "-q", "-m", "project"]);
        let before = git_state(&checkout);
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        assert_eq!(git_state(&checkout), before, "the checkout is as it was");
    });
}

/// The typed text of the `target.unsafe` refusal, and its advice, as
/// `refuse_writes_outside_a_worktree` words them: asserted in full so the
/// tests fail if the refusal is removed or its wording changes.
const UNSAFE: &str = "which may write files, outside a worktree, so it would write in the \
                      project's own checkout";
const UNSAFE_ADVICE: &str = "run it from a workflow step with `worktree: true` and a `base` \
                            commit, or make the atom read-only";

/// A `workspace-write` Codex atom named directly is a step with no worktree,
/// so it would write in the developer's checkout: `atomic run` refuses it with
/// `target.unsafe` before the run starts, and the checkout is left exactly as
/// it was.
#[test]
fn a_writing_codex_atom_run_directly_in_a_checkout_is_refused() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("unsafe-direct");
        let checkout = dir.join("dev");
        let project = project(&checkout.join("examples/coding"));
        git_in(&checkout, &["init", "-q"]);
        git_in(&checkout, &["add", "."]);
        git_in(&checkout, &["commit", "-q", "-m", "project"]);
        // A JSON object, so the run reaches the write control; its missing
        // `base` is never checked, because the control refuses before the
        // input is. With the control removed, the run stops at the input
        // check — it never reaches a model.
        let data = json!({"task": "multiply"}).to_string();
        let before = git_state(&checkout);
        let atom_file = project.join("atoms/fix-code.atom.astro");
        let written_before = std::fs::read(&atom_file).unwrap();
        let out = atomic(
            &dir,
            &project,
            &["--json", "run", "fix-code", "--data", &data],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "target.unsafe", "{error}");
        let description = error["description"].as_str().expect("a description");
        assert_eq!(
            description,
            format!("step `run` runs atom `fix-code`, {UNSAFE}"),
            "{error}"
        );
        assert_eq!(error["action"], UNSAFE_ADVICE, "{error}");
        // Nothing ran, so the checkout is as it was: its refs, its worktree
        // list, its working tree with every untracked file, and the bytes of
        // the very atom that was refused.
        assert_eq!(git_state(&checkout), before, "the checkout is as it was");
        assert_eq!(
            std::fs::read(&atom_file).unwrap(),
            written_before,
            "the refused atom's file is unchanged"
        );
        // And without `--json`, the same refusal reaches a person: a
        // description naming the write, and its advice on its own line.
        let out = atomic(
            &dir,
            &project,
            &["run", "fix-code", "--data", &data],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("error: step `run` runs atom `fix-code`,")
                && stderr.contains(UNSAFE)
                && stderr.contains(&format!("\n  to fix: {UNSAFE_ADVICE}")),
            "{stderr}"
        );
        assert_eq!(
            git_state(&checkout),
            before,
            "the plain run left the checkout as it was"
        );
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// The same `workspace-write` Codex atom as a workflow step that names no
/// `worktree: true`: refused with `target.unsafe`, and the checkout is left as
/// it was.
#[test]
fn a_writing_codex_atom_in_a_step_without_a_worktree_is_refused() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("unsafe-step");
        let checkout = dir.join("dev");
        let project = project(&checkout.join("examples/coding"));
        // `coding`, but its step names no worktree: the same atom, run in a
        // step that would write in the checkout.
        write(
            &project.join("workflows/writable.workflow.yaml"),
            "atomic: 1\nname: writable\n\
             input: { $ref: ../atoms/fix-code.input.schema.json }\n\
             output: { $ref: ../atoms/fix-code.output.schema.json }\n\
             steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n\
             returns: steps.fix.output\n",
        );
        git_in(&checkout, &["init", "-q"]);
        git_in(&checkout, &["add", "."]);
        git_in(&checkout, &["commit", "-q", "-m", "project"]);
        // A JSON object, so the step reaches the write control; its missing
        // `base` is never checked, because the control refuses before the
        // input is. With the control removed the run stops at the input check.
        let data = json!({"task": "multiply"}).to_string();
        let before = git_state(&checkout);
        let out = atomic(
            &dir,
            &project,
            &["--json", "run", "writable", "--data", &data],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "target.unsafe", "{error}");
        assert_eq!(
            error["description"],
            format!("step `fix` runs atom `fix-code`, {UNSAFE}"),
            "{error}"
        );
        assert_eq!(error["action"], UNSAFE_ADVICE, "{error}");
        assert_eq!(git_state(&checkout), before, "the checkout is as it was");
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// `atomic run` executes a Codex atom that writes — its turn runs a command
/// that creates a file — and the developer's checkout is left exactly as it
/// was: the write lands in the step's worktree, which atomic commits in the
/// fixture's snapshot, never in the checkout.
#[test]
fn running_a_writing_codex_atom_leaves_the_checkout_exactly_as_it_was() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("wrote");
        let checkout = dir.join("dev");
        let project = project(&checkout.join("examples/coding"));
        git_in(&checkout, &["init", "-q"]);
        git_in(&checkout, &["add", "."]);
        git_in(&checkout, &["commit", "-q", "-m", "project"]);
        let before = git_state(&checkout);
        let base = fixture(&dir.join("fixture"));
        // The turn writes a file in its worktree and prints it, then answers.
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": "echo written > note.txt; cat note.txt", "yield_time_ms": 20000}),
            },
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "write a note"}).to_string();
        let fixture_arg = dir.join("fixture").to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "coding",
                "--run-id",
                "wrote-1",
                "--data",
                &data,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["output"], json!({"done": true}), "{doc}");
        // The turn's command ran at the top of its worktree and wrote there.
        assert!(
            printed(&model).contains("written"),
            "the turn did not write: {}",
            printed(&model)
        );
        let snapshots = snapshots(&dir);
        assert_eq!(snapshots.len(), 1, "{snapshots:?}");
        let commit = accepted(&snapshots[0], "wrote-1", "fix");
        assert_eq!(
            git_in(&snapshots[0], &["show", &format!("{commit}:note.txt")]),
            "written",
            "the file the turn wrote is in the commit atomic accepted"
        );
        // The developer's checkout is as it was: refs, worktree list, the
        // working tree with every untracked file, and no copy of the file the
        // turn wrote anywhere in it.
        assert_eq!(
            git_state(&checkout),
            before,
            "the checkout is exactly as it was"
        );
        let strayed: Vec<_> = working_files(&checkout)
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "note.txt"))
            .collect();
        assert!(
            strayed.is_empty(),
            "the turn's file reached the checkout: {strayed:?}"
        );
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// Every working file under `dir`, the checkout's own; only its `.git`
/// metadata is skipped, which holds no file the turn could have written.
fn working_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn a_step_never_starts_from_a_commit_another_run_made_on_the_same_fixture() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("other-run");
        let project = project(&dir.join("project"));
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        // The first run's commit is on top of the same bound commit, in the
        // same snapshot, and no git-commit field names it at admission.
        let theirs = accepted(&snapshots(&dir)[0], "fix-1", "fix");
        let model = FakeModel::start(vec![Reply::Text(r#"{"done": true}"#.into())])
            .await
            .unwrap();
        let data = json!({"from": theirs, "task": "multiply"}).to_string();
        let fixture_arg = dir.join("fixture").to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "run",
                "relay",
                "--run-id",
                "relay-1",
                "--data",
                &data,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            text(&out).contains("nor a commit an earlier step of this run made"),
            "{}",
            text(&out)
        );
        assert_eq!(model.calls(), 0, "no turn ran from another run's commit");
        let refs = git_in(
            &snapshots(&dir)[0],
            &["for-each-ref", "refs/atomic/relay-1/"],
        );
        assert!(refs.is_empty(), "{refs}");
    });
}

#[test]
fn nothing_a_hostile_fixtures_configuration_names_ever_runs() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("hostile");
        let project = project(&dir.join("project"));
        let source = dir.join("fixture");
        let markers = dir.join("markers");
        std::fs::create_dir_all(&markers).unwrap();
        let mark =
            |name: &str| format!("#!/bin/sh\ntouch '{}'\ncat\n", markers.join(name).display());
        fixture(&source);
        // Committed, so a checkout of the fixture would give every file the
        // filter.
        write(&source.join(".gitattributes"), "* filter=evil\n");
        git_in(&source, &["add", ".gitattributes"]);
        git_in(&source, &["commit", "-q", "-m", "attributes"]);
        let base = git_in(&source, &["rev-parse", "HEAD"]);
        for hook in [
            "pre-commit",
            "post-commit",
            "post-checkout",
            "reference-transaction",
            "pre-auto-gc",
            "post-rewrite",
            "pre-push",
        ] {
            let path = source.join(".git/hooks").join(hook);
            write(&path, &mark(hook));
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let script = |name: &str| {
            let path = dir.join(format!("{name}.sh"));
            write(&path, &mark(name));
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            path.display().to_string()
        };
        for (key, value) in [
            ("core.fsmonitor", script("fsmonitor")),
            ("filter.evil.clean", script("clean")),
            ("filter.evil.smudge", script("smudge")),
            ("filter.evil.process", script("process")),
            (
                "core.hooksPath",
                source.join(".git/hooks").display().to_string(),
            ),
            ("remote.origin.url", format!("ext::{} %S", script("ext"))),
            (
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*".into(),
            ),
        ] {
            git_in(&source, &["config", key, &value]);
        }
        // The fixture's own git, as a person runs it, would run some of
        // these: the markers prove the test can see them.
        let model = FakeModel::start(vec![Reply::Text(r#"{"done": true}"#.into())])
            .await
            .unwrap();
        let data = json!({"base": base, "task": "multiply"}).to_string();
        let source_arg = source.to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "coding",
                "--run-id",
                "hostile-1",
                "--data",
                &data,
                "--fixture",
                &source_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let ran: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert!(ran.is_empty(), "the fixture's configuration ran: {ran:?}");
        // The positive control: git as a person runs it does run them.
        write(&source.join("touched.txt"), "x\n");
        git_in(&source, &["add", "touched.txt"]);
        let ran: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert!(!ran.is_empty(), "the markers are seen when git runs them");
    });
}

#[test]
fn a_run_killed_after_a_step_goes_on_from_the_snapshot_once_its_source_is_gone() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("resume");
        let project = project(&dir.join("project"));
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply"}).to_string();
        let fixture_arg = dir.join("fixture").to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "run",
                "twice",
                "--run-id",
                "twice-1",
                "--data",
                &data,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ],
            &model.base_url(),
            &[("ATOMIC_FAILPOINT", "pipeline:after:first")],
        )
        .await;
        assert!(
            !out.status.success(),
            "killed after the first step: {}",
            text(&out)
        );
        assert_eq!(model.calls(), 1);
        // The source goes; the run was pinned to its snapshot.
        std::fs::remove_dir_all(dir.join("fixture")).unwrap();
        let out = atomic(
            &dir,
            &project,
            &["runs", "resume", "twice-1"],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 2, "the first step was not run again");
        let snapshot = &snapshots(&dir)[0];
        let first = accepted(snapshot, "twice-1", "first");
        let second = accepted(snapshot, "twice-1", "second");
        assert_eq!(git_in(snapshot, &["rev-parse", &format!("{first}^")]), base);
        assert_eq!(
            git_in(snapshot, &["rev-parse", &format!("{second}^")]),
            first
        );
    });
}
