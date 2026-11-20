// SPDX-License-Identifier: Apache-2.0

//! A coding run through the `atomic` executable, bound to a fixture
//! repository: its worktree step runs a Codex atom under the real
//! `workspace-write` sandbox in a worktree of the fixture's snapshot, and
//! atomic commits there. The developer's checkout, which holds the project,
//! and the fixture repository itself are left exactly as they were.
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

/// A project with one codex atom that may write, run by a workflow in a
/// worktree at its mapped base.
fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
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
    project
}

#[test]
fn a_coding_run_works_in_the_snapshot_and_leaves_both_repositories_as_they_were() {
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done": true}"#.into())])
            .await
            .unwrap();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-fixture-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let project = project(&dir);
        git_in(&project, &["init", "-q"]);
        git_in(&project, &["add", "."]);
        git_in(&project, &["commit", "-q", "-m", "project"]);
        let fixture = dir.join("fixture");
        write(&fixture.join("price.txt"), "125\n");
        git_in(&fixture, &["init", "-q"]);
        git_in(&fixture, &["add", "price.txt"]);
        git_in(&fixture, &["commit", "-q", "-m", "seed"]);
        let base = git_in(&fixture, &["rev-parse", "HEAD"]);
        let (checkout, source) = (git_state(&project), git_state(&fixture));

        let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
        let fixture_arg = fixture.to_string_lossy().into_owned();
        let (d, p, url) = (dir.clone(), project.clone(), model.base_url());
        let out = tokio::task::spawn_blocking(move || {
            Command::new(ATOMIC)
                .args([
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
                ])
                .current_dir(&p)
                .env("HOME", d.join("home"))
                .env("ATOMIC_HOME", d.join("atomic-home"))
                .env("USER", "dev")
                .env("ATOMIC_MODEL_BASE_URL", url)
                .env("ATOMIC_MODEL", "synthetic")
                .env_remove("ATOMIC_DATABASE_URL")
                .env_remove("CODEX_HOME")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["output"], json!({"done": true}), "{doc}");
        assert_eq!(model.calls(), 1);

        // atomic committed the step's work in the snapshot, on the base.
        let snapshots: Vec<PathBuf> = std::fs::read_dir(dir.join("atomic-home/projects"))
            .unwrap()
            .flat_map(|p| {
                std::fs::read_dir(p.unwrap().path().join("fixtures"))
                    .into_iter()
                    .flatten()
            })
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(snapshots.len(), 1, "{snapshots:?}");
        let snapshot = &snapshots[0];
        assert!(
            snapshot.ends_with(format!("{base}.git")),
            "{}",
            snapshot.display()
        );
        let refs = git_in(
            snapshot,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/atomic/fix-1/",
            ],
        );
        let accepted = refs
            .lines()
            .find(|l| l.starts_with("refs/atomic/fix-1/steps/fix/"))
            .unwrap_or_else(|| panic!("no accepted commit in {refs}"))
            .split(' ')
            .nth(1)
            .unwrap()
            .to_owned();
        assert_eq!(
            git_in(snapshot, &["rev-parse", &format!("{accepted}^")]),
            base
        );

        // Neither the developer's checkout nor the fixture changed at all.
        assert_eq!(git_state(&project), checkout, "the checkout is as it was");
        assert_eq!(git_state(&fixture), source, "the fixture is as it was");
        let _ = std::fs::remove_dir_all(&dir);
    });
}
