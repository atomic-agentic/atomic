// SPDX-License-Identifier: Apache-2.0

//! A command an atom runs gets the platform's core variables and what the
//! atom declares, and never atomic's own environment: with database URLs, a
//! Postgres password, a `*TOKEN` variable and a project secret set in this
//! process, an exec atom's command finds none of them, on the local backend
//! and through a runner process, and neither does a command a model runs in
//! a Codex turn. A variable the atom declares is present in each.
//!
//! The Codex turn also shows that only the backend places a turn's shell:
//! with `CODEX_EXEC_SERVER_URL` and the `CODEX_EXEC_SERVER_NOISE_*`
//! variables set in atomic's environment and an `environments.toml` in the
//! Codex home, all naming exec-servers that do not exist, the command still
//! runs on this machine. And no shell startup file runs: with a `HOME` whose
//! profiles export a marker, the command does not see it, and its `HOME` and
//! `TMPDIR` are its workspace's scratch directory and a directory inside it.
//!
//! The Codex turn needs Codex's Linux sandbox, so that part runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot
//! start fails the test.

#![recursion_limit = "512"]

#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
mod support;

use atomic_core::runtime::{self, EffectIdentity};
use atomic_core::testing::on_codex_runtime;
use atomic_schema::registry::Registry;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What atomic's own environment holds, by name and value. `DATABASE_URL`
/// and `PGPASSWORD` match no pattern: only inheriting the core variables
/// keeps them out.
const SECRETS: [(&str, &str); 5] = [
    (
        "ATOMIC_DATABASE_URL",
        "postgres://atomic:hunter2-db@db.invalid/atomic",
    ),
    ("DATABASE_URL", "postgres://app:hunter2-app@db.invalid/app"),
    ("PGPASSWORD", "hunter2-pg"),
    ("GITHUB_TOKEN", "ghp_hunter2token"),
    ("ATOMIC_SECRET_DEPLOY_KEY", "hunter2-deploy"),
];

/// Exec-servers that do not exist, named everywhere Codex looks for one.
const ELSEWHERE: [(&str, &str); 4] = [
    ("CODEX_EXEC_SERVER_URL", "ws://127.0.0.1:9/nowhere"),
    (
        "CODEX_EXEC_SERVER_NOISE_REGISTRY_URL",
        "https://127.0.0.1:9/registry",
    ),
    ("CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID", "elsewhere"),
    ("CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN", "hunter2-noise"),
];

/// What the developer's own shell profiles export.
const MARKER: &str = "ATOMIC_TEST_PROFILE_MARKER";

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
        // `ATOMIC_DATABASE_URL` holds `DATABASE_URL`: match whole names.
        assert!(
            !seen.contains(&format!("\"{name}=")) && !seen.contains(&format!("\\n{name}=")),
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

// needs_sandbox (its Codex turn)
#[test]
fn atom_commands_get_only_core_and_declared_variables_on_every_backend() {
    #[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
    support::require_sandbox();
    // A HOME whose shell profiles export a marker.
    let home = scratch("home");
    for profile in [
        ".profile",
        ".bash_profile",
        ".bashrc",
        ".zshenv",
        ".zprofile",
    ] {
        std::fs::write(home.join(profile), format!("export {MARKER}=leaked\n")).expect("profile");
    }
    let home = home.display().to_string();
    // Before any other thread starts: this test is alone in its binary.
    for (name, value) in SECRETS
        .into_iter()
        .chain(ELSEWHERE)
        .chain([("HOME", home.as_str())])
    {
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
            (
                "the local backend",
                Arc::new(atomic_exec::LocalBackend::in_dir(
                    scratch("exec-state").join("scratch"),
                )),
            ),
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
                &runtime::Artifacts::none(),
            )
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            let seen = out["env"].to_string();
            assert_clean(what, &seen);
            assert!(!seen.contains(MARKER), "{what}: a profile ran: {seen}");
        }
        #[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
        codex_turn().await;
    });
}

/// A command the model runs in a Codex turn, as its tool output reached the
/// model.
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
async fn codex_turn() {
    use atomic_core::codex::{CodexPaths, ModelEndpoint, TurnSpec};
    use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
    use atomic_core::testing::{FakeModel, Reply};
    let dir = scratch("codex");
    for sub in ["cwd", "codex-home", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    std::fs::write(
        dir.join("codex-home/environments.toml"),
        "default = \"elsewhere\"\ninclude_local = false\n\n[[environments]]\n\
         id = \"elsewhere\"\nurl = \"ws://127.0.0.1:9/nowhere\"\n",
    )
    .expect("environments.toml");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({
                "cmd": "echo ran-here-$((6*7)); echo shell=$BASH_VERSION; \
                        if shopt -q login_shell; then echo login=yes; else echo login=no; fi; env",
                "workdir": dir.join("cwd"),
                "yield_time_ms": 10000
            }),
        },
        Reply::Text("Done.".into()),
    ])
    .await
    .expect("fake model");
    let scratch_root = dir.join("state/scratch");
    let runtime = CodexRuntime {
        paths: CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            dir.join("bin/codex-linux-sandbox"),
        ),
        codex_home: dir.join("codex-home"),
        cwd: dir.join("cwd"),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        defaults: Default::default(),
        artifacts: Artifacts::none(),
        meter: atomic_core::usage::Metering::none(),
        guard: atomic_core::codex::StateGuard::NoProjectState,
        backend: atomic_exec::Backend::connect(atomic_exec::LocalBackend::in_dir(
            scratch_root.clone(),
        ))
        .expect("connects"),
        caches: None,
    };
    let answer = run_turn(
        &runtime,
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
        output.contains("ran-here-42"),
        "the command did not run on this machine: {output}"
    );
    assert!(
        output.contains("PATH="),
        "the command ran with no core variables: {output}"
    );
    assert_clean("a Codex turn", &output);
    for (name, value) in ELSEWHERE {
        assert!(
            !output.contains(value),
            "{name} reached the command: {output}"
        );
    }
    assert!(!output.contains(MARKER), "a shell profile ran: {output}");
    // HOME and TMPDIR are the workspace's own scratch directory. The output
    // is a JSON string, so its lines end in an escaped newline.
    let value = |name: &str| {
        output
            .split("\\n")
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} in {output}"))
            .to_owned()
    };
    // Not a login shell: the check runs in bash, so it is not vacuous.
    assert!(
        !value("shell").is_empty(),
        "the command did not run in bash: {output}"
    );
    assert!(output.contains("login=no"), "a login shell ran: {output}");
    // HOME and ZDOTDIR are the attempt's own scratch directory, so no
    // shell, zsh included, finds startup files there; TMPDIR is the turn's
    // own too, inside it, and not the backend's shared one.
    let (home, zdotdir, tmp) = (value("HOME"), value("ZDOTDIR"), value("TMPDIR"));
    assert_eq!(home, zdotdir);
    assert_eq!(tmp, format!("{home}/tmp"));
    assert!(!output.contains("ATOMIC_TURN_TMPDIR"), "{output}");
    for dir in [&home, &tmp] {
        assert!(
            Path::new(dir).starts_with(&scratch_root),
            "{dir} is not under {}",
            scratch_root.display()
        );
    }
}
