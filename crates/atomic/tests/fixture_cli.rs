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

/// git in `dir`, as a person runs it.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=dev",
            "-c",
            "user.email=dev@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_state(dir: &Path) -> String {
    format!(
        "{}\n{}\n{}",
        git_in(dir, &["for-each-ref"]),
        git_in(dir, &["worktree", "list", "--porcelain"]),
        git_in(dir, &["status", "--porcelain", "--untracked-files=all"])
    )
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

#[test]
fn a_project_outside_any_repository_runs_its_coding_step_in_the_snapshot() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("outside");
        let project = project(&dir.join("project"));
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
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
