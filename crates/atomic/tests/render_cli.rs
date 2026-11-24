// SPDX-License-Identifier: Apache-2.0

//! Compare the public prompt preview with the first request of a real CLI run.

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
mod support;

fn project(kind: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-render-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("project/atoms")).expect("project atoms");
    std::fs::create_dir_all(root.join("home")).expect("private home");
    let project = root.join("project");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: render-comparison\n",
    )
    .expect("manifest");
    std::fs::write(
        project.join("atoms/note.atom.astro"),
        format!("---\n\
         import type {{ Props, Input, Output }} from '../.atomic/types/note';\n\
         import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
         const {{ input }}: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = {{ kind: '{kind}', sandbox: 'read-only' }};\n\
         ---\n\
         Give a short answer to this note.\n\
         Note: {{input.text}}\n"),
    )
    .expect("atom");
    std::fs::write(
        project.join("atoms/note.input.schema.json"),
        r#"{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string"}}}"#,
    )
    .expect("input schema");
    std::fs::write(
        project.join("atoms/note.output.schema.json"),
        r#"{"type":"object","additionalProperties":false,"required":["done"],"properties":{"done":{"type":"boolean"}}}"#,
    )
    .expect("output schema");
    std::fs::write(project.join("input.json"), r#"{"text":"the door sticks"}"#).expect("input");
    root
}

fn atomic(root: &Path, args: &[&str], endpoint: &str) -> Output {
    atomic_with_endpoint(root, args, Some(endpoint))
}

fn atomic_with_endpoint(root: &Path, args: &[&str], endpoint: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .args(args)
        .current_dir(root.join("project"))
        .env("HOME", root.join("home"))
        .env("ATOMIC_HOME", root.join("home/atomic"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("ATOMIC_MODEL_BASE_URL")
        .env_remove("ATOMIC_MODEL");
    if let Some(endpoint) = endpoint {
        command
            .env("ATOMIC_MODEL_BASE_URL", endpoint)
            .env("ATOMIC_MODEL", "synthetic");
    }
    command.output().expect("atomic process")
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8")
}

#[test]
fn prompt_snapshot_changes_fail_check() {
    let root = project("lite");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("fixtures/note")).expect("fixtures");
    std::fs::create_dir_all(project.join("snapshots")).expect("snapshots");
    std::fs::write(
        project.join("fixtures/note/ordinary.input.json"),
        r#"{"text":"the door sticks"}"#,
    )
    .expect("case input");
    std::fs::write(
        project.join("fixtures/note/second.input.json"),
        r#"{"text":"the window rattles"}"#,
    )
    .expect("second input");
    let endpoint = "http://127.0.0.1:9/v1";
    let snapshot = success(atomic(
        &root,
        &[
            "render",
            "atoms/note.atom.astro",
            "--input",
            "fixtures/note/ordinary.input.json",
        ],
        endpoint,
    ));
    std::fs::write(
        project.join("snapshots/note__ordinary.prompt.json"),
        snapshot,
    )
    .expect("snapshot");
    let missing = atomic(&root, &["check"], endpoint);
    assert!(!missing.status.success(), "missing snapshot must fail");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("note__second.prompt.json"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    let second = success(atomic(
        &root,
        &[
            "render",
            "atoms/note.atom.astro",
            "--input",
            "fixtures/note/second.input.json",
        ],
        endpoint,
    ));
    std::fs::write(project.join("snapshots/note__second.prompt.json"), second)
        .expect("second snapshot");
    success(atomic(&root, &["check"], endpoint));
    let source = project.join("atoms/note.atom.astro");
    let changed = std::fs::read_to_string(&source)
        .expect("atom source")
        .replace("Give a short answer", "Give a detailed answer");
    std::fs::write(source, changed).expect("changed atom");
    let changed = atomic(&root, &["check"], endpoint);
    assert!(!changed.status.success(), "changed prompt must fail");
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("note__ordinary.prompt.json"),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn prompt_snapshot_link_loop_refuses() {
    use std::os::unix::fs::symlink;

    let root = project("lite");
    let path = root.join("project/snapshots");
    symlink("snapshots", &path).expect("self-referential snapshot directory");
    let result = atomic(&root, &["check"], "http://127.0.0.1:9/v1");
    assert!(!result.status.success(), "link loop must refuse");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("regular directory"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn helper_prompt_snapshot_and_render_refuse_without_recorded_clock() {
    let root = project("lite");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("atoms/helpers")).expect("helpers");
    std::fs::write(
        project.join("atoms/helpers/clock.ts"),
        "export const clock = (text: string): string => `${text}:${Date.now()}:${Math.random()}`;\n",
    )
    .expect("helper");
    let source = project.join("atoms/note.atom.astro");
    let changed = std::fs::read_to_string(&source)
        .expect("source")
        .replace(
            "const { input }: Props = Astro.props;",
            "import { clock } from './helpers/clock';\nconst { input }: Props = Astro.props;",
        )
        .replace("Note: {input.text}", "Note: {clock(input.text)}");
    std::fs::write(&source, changed).expect("helper atom");
    std::fs::create_dir_all(project.join("fixtures/note")).expect("fixtures");
    std::fs::create_dir_all(project.join("snapshots")).expect("snapshots");
    std::fs::write(
        project.join("fixtures/note/ordinary.input.json"),
        r#"{"text":"the door sticks"}"#,
    )
    .expect("input");
    std::fs::write(project.join("snapshots/note__ordinary.prompt.json"), "{}\n").expect("snapshot");
    let preview = atomic(
        &root,
        &[
            "render",
            "atoms/note.atom.astro",
            "--input",
            "fixtures/note/ordinary.input.json",
        ],
        "http://127.0.0.1:9/v1",
    );
    assert!(!preview.status.success(), "unclocked render must refuse");
    assert!(
        String::from_utf8_lossy(&preview.stderr).contains("needs a recorded step clock"),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let inspect = atomic(
        &root,
        &[
            "render",
            "atoms/note.atom.astro",
            "--input",
            "fixtures/note/ordinary.input.json",
            "--preview-helpers",
        ],
        "http://127.0.0.1:9/v1",
    );
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    assert!(
        String::from_utf8_lossy(&inspect.stderr).contains("may differ from a live prompt"),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspect: Value = serde_json::from_slice(&inspect.stdout).expect("helper preview roles");
    assert!(
        inspect["user"]
            .as_str()
            .expect("user role")
            .contains("the door sticks"),
        "{inspect}"
    );
    let result = atomic(&root, &["check"], "http://127.0.0.1:9/v1");
    assert!(!result.status.success(), "unclocked snapshot must refuse");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("need a recorded step clock"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn render_matches_the_first_live_lite_request_without_calling_the_model() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("fake model");
        let root = project("lite");
        let endpoint = model.base_url();
        let render_root = root.clone();
        let render_endpoint = endpoint.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            success(atomic(
                &render_root,
                &[
                    "render",
                    "atoms/note.atom.astro",
                    "--input",
                    "input.json",
                    "--step-id",
                    "note",
                ],
                &render_endpoint,
            ))
        })
        .await
        .expect("render process");
        assert_eq!(model.calls(), 0, "a preview makes no model request");
        let rendered: Value = serde_json::from_str(&rendered).expect("rendered roles");
        let run_root = root.clone();
        let run_endpoint = endpoint.clone();
        let result = tokio::task::spawn_blocking(move || {
            success(atomic(
                &run_root,
                &[
                    "--json",
                    "run",
                    "note",
                    "--run-id",
                    "preview",
                    "--data",
                    r#"{"text":"the door sticks"}"#,
                ],
                &run_endpoint,
            ))
        })
        .await
        .expect("run process");
        assert!(result.contains("done"), "{result}");
        assert_eq!(model.calls(), 1, "one first generation, no repair");
        let requests = model.requests();
        let messages = requests[0]["messages"].as_array().expect("chat messages");
        assert_eq!(messages.len(), 2, "first request has the two prompt roles");
        assert_eq!(
            messages[0],
            json!({"role":"system","content":rendered["developer"]})
        );
        assert_eq!(
            messages[1],
            json!({"role":"user","content":rendered["user"]})
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    });
}

/// Codex on a non-OpenAI endpoint adds the lowered output schema to the
/// developer instructions. A preview must include those exact bytes too.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn render_matches_non_openai_codex_developer_instructions() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("fake model");
        let root = project("codex");
        let endpoint = model.base_url();
        let render_root = root.clone();
        let render_endpoint = endpoint.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            success(atomic(
                &render_root,
                &[
                    "render",
                    "atoms/note.atom.astro",
                    "--input",
                    "input.json",
                    "--step-id",
                    "note",
                ],
                &render_endpoint,
            ))
        })
        .await
        .expect("render process");
        assert_eq!(model.calls(), 0, "a preview makes no model request");
        let rendered: Value = serde_json::from_str(&rendered).expect("rendered roles");
        let run_root = root.clone();
        let run_endpoint = endpoint.clone();
        let result = tokio::task::spawn_blocking(move || {
            success(atomic(
                &run_root,
                &[
                    "--json",
                    "run",
                    "note",
                    "--run-id",
                    "preview",
                    "--data",
                    r#"{"text":"the door sticks"}"#,
                ],
                &run_endpoint,
            ))
        })
        .await
        .expect("run process");
        assert!(result.contains("done"), "{result}");
        assert_eq!(model.calls(), 1, "one first generation, no repair");
        let request = &model.requests()[0];
        let developer = request["input"]
            .as_array()
            .expect("Codex input messages")
            .iter()
            .filter(|message| message["role"] == "developer")
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .find(|text| text.contains("Give a short answer to this note."))
            .expect("the atom's developer instructions");
        assert_eq!(rendered["developer"].as_str(), Some(developer));
        assert!(developer.contains("JSON Schema"), "{developer}");
        std::fs::remove_dir_all(root).expect("cleanup");
    });
}

/// The profile flag must resolve the same model endpoint for render and run.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn render_profile_matches_live_codex_endpoint() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("fake model");
        let root = project("codex");
        let project = root.join("project");
        std::fs::write(
            project.join("atomic.yaml"),
            "atomic: 1\nname: render-comparison\nmodel:\n  base_url: https://api.openai.com/v1\n  name: synthetic\n",
        )
        .expect("default model");
        std::fs::write(
            project.join("atomic.nightly.yaml"),
            format!(
                "model:\n  base_url: {}\n  name: synthetic\n",
                model.base_url()
            ),
        )
        .expect("selected model");
        let default = success(atomic_with_endpoint(
            &root,
            &["render", "atoms/note.atom.astro", "--input", "input.json"],
            None,
        ));
        assert!(!default.contains("JSON Schema"), "{default}");
        let render_root = root.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            success(atomic_with_endpoint(
                &render_root,
                &[
                    "render",
                    "atoms/note.atom.astro",
                    "--input",
                    "input.json",
                    "--profile",
                    "nightly",
                    "--step-id",
                    "note",
                ],
                None,
            ))
        })
        .await
        .expect("render process");
        assert_eq!(model.calls(), 0, "render must make no model call");
        let rendered: Value = serde_json::from_str(&rendered).expect("rendered roles");
        let run_root = root.clone();
        let result = tokio::task::spawn_blocking(move || {
            success(atomic_with_endpoint(
                &run_root,
                &[
                    "--json",
                    "run",
                    "note",
                    "--profile",
                    "nightly",
                    "--run-id",
                    "preview",
                    "--data",
                    r#"{"text":"the door sticks"}"#,
                ],
                None,
            ))
        })
        .await
        .expect("run process");
        assert!(result.contains("done"), "{result}");
        assert_eq!(model.calls(), 1, "selected endpoint receives one turn");
        let request = &model.requests()[0];
        let developer = request["input"]
            .as_array()
            .expect("Codex input messages")
            .iter()
            .filter(|message| message["role"] == "developer")
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .find(|text| text.contains("Give a short answer to this note."))
            .expect("atom developer instructions");
        assert_eq!(rendered["developer"].as_str(), Some(developer));
        assert!(developer.contains("JSON Schema"), "{developer}");
        std::fs::create_dir_all(project.join("fixtures/note")).expect("fixtures");
        std::fs::create_dir_all(project.join("snapshots")).expect("snapshots");
        std::fs::write(
            project.join("fixtures/note/ordinary.input.json"),
            r#"{"text":"the door sticks"}"#,
        )
        .expect("fixture");
        let selected_snapshot = success(atomic_with_endpoint(
            &root,
            &[
                "render",
                "atoms/note.atom.astro",
                "--input",
                "fixtures/note/ordinary.input.json",
                "--profile",
                "nightly",
            ],
            None,
        ));
        std::fs::write(
            project.join("snapshots/note__ordinary.prompt.json"),
            selected_snapshot,
        )
        .expect("selected snapshot");
        success(atomic_with_endpoint(
            &root,
            &["check", "--profile", "nightly"],
            None,
        ));
        let wrong_profile = atomic_with_endpoint(&root, &["check"], None);
        assert!(
            !wrong_profile.status.success(),
            "default endpoint has a different prompt"
        );
        assert!(
            String::from_utf8_lossy(&wrong_profile.stderr).contains("prompt snapshot changed"),
            "{}",
            String::from_utf8_lossy(&wrong_profile.stderr)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    });
}

/// A command-line endpoint override changes Codex's schema guidance on both
/// preview and live dispatch, even when the project default is OpenAI.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn render_model_base_url_matches_live_codex_override() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("fake model");
        let root = project("codex");
        std::fs::write(
            root.join("project/atomic.yaml"),
            "atomic: 1\nname: render-comparison\nmodel:\n  base_url: https://api.openai.com/v1\n  name: synthetic\n",
        )
        .expect("default endpoint");
        let endpoint = model.base_url();
        let render_root = root.clone();
        let render_endpoint = endpoint.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            success(atomic_with_endpoint(
                &render_root,
                &[
                    "render",
                    "atoms/note.atom.astro",
                    "--input",
                    "input.json",
                    "--step-id",
                    "note",
                    "--model-base-url",
                    &render_endpoint,
                ],
                None,
            ))
        })
        .await
        .expect("render process");
        assert_eq!(model.calls(), 0, "render must make no model call");
        let rendered: Value = serde_json::from_str(&rendered).expect("rendered roles");
        let run_root = root.clone();
        let run_endpoint = endpoint.clone();
        let result = tokio::task::spawn_blocking(move || {
            success(atomic_with_endpoint(
                &run_root,
                &[
                    "--json",
                    "run",
                    "note",
                    "--run-id",
                    "preview",
                    "--data",
                    r#"{"text":"the door sticks"}"#,
                    "--model-base-url",
                    &run_endpoint,
                ],
                None,
            ))
        })
        .await
        .expect("run process");
        assert!(result.contains("done"), "{result}");
        assert_eq!(model.calls(), 1, "overridden endpoint receives one turn");
        let developer = model.requests()[0]["input"]
            .as_array()
            .expect("Codex input messages")
            .iter()
            .filter(|message| message["role"] == "developer")
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .find(|text| text.contains("Give a short answer to this note."))
            .expect("atom developer instructions")
            .to_owned();
        assert_eq!(rendered["developer"].as_str(), Some(developer.as_str()));
        assert!(developer.contains("JSON Schema"), "{developer}");
        std::fs::remove_dir_all(root).expect("cleanup");
    });
}
