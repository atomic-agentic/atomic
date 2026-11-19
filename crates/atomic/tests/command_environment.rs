// SPDX-License-Identifier: Apache-2.0

//! A command an atom runs gets the platform's core variables and what the
//! atom declares, and never atomic's own environment: with a database URL,
//! a `*TOKEN` variable and a project secret set in this process, an exec
//! atom's command finds none of them, on the local backend and through a
//! runner process, and neither does a command a model runs in a Codex turn.
//! A variable the atom declares is present in each.
//!
//! The Codex turn needs Codex's Linux sandbox, so that part runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot
//! start fails the test.

#![recursion_limit = "512"]

use atomic_core::runtime::{self, EffectIdentity};
use atomic_core::testing::on_codex_runtime;
use atomic_schema::registry::Registry;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What atomic's own environment holds, by name and value.
const SECRETS: [(&str, &str); 3] = [
    (
        "ATOMIC_DATABASE_URL",
        "postgres://atomic:hunter2-db@db.invalid/atomic",
    ),
    ("GITHUB_TOKEN", "ghp_hunter2token"),
    ("ATOMIC_SECRET_DEPLOY_KEY", "hunter2-deploy"),
];

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-command-env-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(dir).expect("canonical")
}

/// A project with one exec atom whose command reports every variable it
/// sees, by name and value, and declares `DECLARED=yes`.
fn exec_atom(dir: &Path) -> (atomic_core::atom::Atom, atomic_core::compile::Compiled) {
    for sub in ["atoms", "tools"] {
        std::fs::create_dir_all(dir.join(sub)).expect("project");
    }
    std::fs::write(dir.join("atomic.yaml"), "atomic: 1\nname: command-env\n").expect("manifest");
    std::fs::write(
        dir.join("tools/envs.py"),
        "import json, os, sys\njson.load(sys.stdin)\n\
         json.dump({'env': [k + '=' + v for k, v in os.environ.items()]}, sys.stdout)\n",
    )
    .expect("script");
    std::fs::write(
        dir.join("atoms/envs.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/envs';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Report the environment.', \
         kind: 'exec', exec: { command: ['python3', 'tools/envs.py'] }, env: { DECLARED: 'yes' } };\n---\n",
    )
    .expect("atom");
    std::fs::write(
        dir.join("atoms/envs.input.schema.yaml"),
        "type: object\nadditionalProperties: false\nproperties: {}\n",
    )
    .expect("input");
    std::fs::write(
        dir.join("atoms/envs.output.schema.yaml"),
        "type: object\nadditionalProperties: false\nrequired: [env]\n\
         properties:\n  env: {type: array, items: {type: string}}\n",
    )
    .expect("output");
    let mut registry = Registry::new();
    let atom =
        atomic_core::atom::load(&dir.join("atoms/envs.atom.astro"), &mut registry).expect("loads");
    let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");
    (atom, compiled)
}

/// `seen` names none of atomic's own variables and holds none of their
/// values, and holds `DECLARED=yes`.
fn assert_clean(what: &str, seen: &str) {
    for (name, value) in SECRETS {
        assert!(
            !seen.contains(name),
            "{what}: {name} reached the command: {seen}"
        );
        assert!(
            !seen.contains(value),
            "{what}: {name}'s value reached the command: {seen}"
        );
    }
    assert!(
        seen.contains("DECLARED=yes"),
        "{what}: the declared variable is missing: {seen}"
    );
}

#[test]
fn atom_commands_get_only_core_and_declared_variables_on_every_backend() {
    // Before any other thread starts: this test is alone in its binary.
    for (name, value) in SECRETS {
        // SAFETY: the process has one thread here.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var(name, value);
        }
    }
    on_codex_runtime(async {
        let dir = scratch("exec");
        let (atom, compiled) = exec_atom(&dir);
        let backends: [(&str, Arc<dyn atomic_exec::ExecutionBackend>); 2] = [
            ("the local backend", Arc::new(atomic_exec::LocalBackend)),
            (
                "a runner process",
                Arc::new(atomic_exec::runner::ProcessRunner::atomic(PathBuf::from(
                    env!("CARGO_BIN_EXE_atomic"),
                ))),
            ),
        ];
        for (what, backend) in backends {
            let (out, _) = runtime::run_exec(
                &atom,
                &compiled,
                &json!({}),
                &EffectIdentity {
                    run_id: "env-1".into(),
                    step_id: "envs".into(),
                },
                backend,
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_clean(what, &out["env"].to_string());
        }
        #[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
        codex_turn().await;
    });
}

/// A command the model runs in a Codex turn, as its tool output reached the
/// model.
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
async fn codex_turn() {
    use atomic_core::codex::{self, CodexPaths, ModelEndpoint, TurnSpec};
    use atomic_core::testing::{FakeModel, Reply};
    let dir = scratch("codex");
    for sub in ["cwd", "codex-home", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": "env", "workdir": dir.join("cwd"), "yield_time_ms": 10000}),
        },
        Reply::Text("Done.".into()),
    ])
    .await
    .expect("fake model");
    let answer = codex::run(
        &CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            dir.join("bin/codex-linux-sandbox"),
        ),
        &dir.join("codex-home"),
        &dir.join("cwd"),
        &ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        &TurnSpec {
            prompt: "Show the environment.".into(),
            env: [("DECLARED".to_owned(), "yes".to_owned())].into(),
            ..TurnSpec::default()
        },
    )
    .await
    .expect("the turn runs");
    assert_eq!(answer, "Done.");
    let requests = model.requests();
    let follow_up = requests.get(1).expect("Codex answered the tool call");
    let output: Vec<String> = follow_up["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].to_string())
        .collect();
    assert!(!output.is_empty(), "no command output: {follow_up}");
    let output = output.join("\n");
    assert!(
        output.contains("PATH="),
        "the command ran with no core variables: {output}"
    );
    assert_clean("a Codex turn", &output);
}
