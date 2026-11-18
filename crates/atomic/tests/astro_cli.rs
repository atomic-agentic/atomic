// SPDX-License-Identifier: Apache-2.0

//! Production CLI regression tests for Astro project and context boundaries.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "atomic-cli-review-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("fixture");
        Self(dir)
    }
    fn run(&self, cwd: &Path, args: &[&str], region: &str) -> Output {
        // Every state, cache and key directory is beneath the fixture's home,
        // as a user's are beneath theirs, and none is inherited from the caller.
        Command::new(env!("CARGO_BIN_EXE_atomic"))
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.0.join("home"))
            .env("ATOMIC_HOME", self.0.join("home/.local/share/atomic"))
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("SUPPORT_REGION", region)
            .output()
            .expect("CLI process")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8")
}
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("directory");
    for entry in std::fs::read_dir(source).expect("source") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".atomic" {
            continue;
        }
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

#[test]
fn render_and_check_share_project_schema_discovery() {
    let fixture = Fixture::new("schema-collection");
    let project = fixture.0.join("project");
    copy_tree(&repository().join("examples/08-templating"), &project);
    success(fixture.run(&project, &["check", "atoms/triage-findings.atom.astro"], ""));
    let input = std::fs::read_dir(project.join("fixtures/triage-findings"))
        .expect("fixtures")
        .map(|entry| entry.expect("fixture").path())
        .find(|file| file.to_string_lossy().ends_with(".input.json"))
        .expect("input");
    let result = success(fixture.run(
        &project,
        &[
            "render",
            "atoms/triage-findings.atom.astro",
            "--input",
            input.to_str().expect("path"),
        ],
        "",
    ));
    let result: serde_json::Value = serde_json::from_str(&result).expect("prompt");
    assert!(!result["user"].as_str().expect("user").is_empty());
}

#[test]
fn previews_follow_edits_while_explicit_runs_keep_their_checkpoint() {
    let fixture = Fixture::new("preview");
    let project = fixture.0.join("project");
    copy_tree(
        &repository().join("examples/12-execution-context"),
        &project,
    );
    let args = [
        "render",
        "atoms/context-reply.atom.astro",
        "--input",
        "fixtures/context-reply/ordinary.input.json",
        "--context-settings",
        "context.settings.json",
    ];
    let first = success(fixture.run(&project, &args, "before-edit"));
    assert!(first.contains("before-edit"));
    let atom = project.join("atoms/context-reply.atom.astro");
    let source = std::fs::read_to_string(&atom).expect("atom");
    std::fs::write(&atom, format!("{source}\nPreview edit.\n")).expect("edit");
    let second = success(fixture.run(&project, &args, "after-edit"));
    assert!(second.contains("after-edit") && second.contains("Preview edit."));
    let named = [&args[..], &["--run-id", "frozen", "--profile", "test"]].concat();
    let first_named = success(fixture.run(&project, &named, "frozen-region"));
    assert!(first_named.contains("frozen-region"));
    let restored = success(fixture.run(
        &project,
        &["render", args[1], "--input", args[3], "--run-id", "frozen"],
        "new-region",
    ));
    assert!(restored.contains("frozen-region") && !restored.contains("new-region"));
    std::fs::write(&atom, format!("{source}\nAnother edit.\n")).expect("edit");
    let mismatch = fixture.run(&project, &named, "new-region");
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("version mismatch"));
}

#[test]
fn standalone_home_atom_works_with_compiler_cache_beneath_home() {
    let fixture = Fixture::new("home");
    let home = fixture.0.join("home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(home.join("hello.atom.astro"), "---\nimport type {Props} from './.atomic/types/hello';\nimport Rules from './Rules.astro';\nconst {input}:Props=Astro.props;\n---\n<Rules />\n{input.text}\n").expect("atom");
    std::fs::write(
        home.join("Rules.astro"),
        "<rubric>Use this local component.</rubric>",
    )
    .expect("component");
    std::fs::write(home.join("unrelated-secret"), "never-read-this").expect("unrelated file");
    let schema = r#"{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string"}}}"#;
    for role in ["input", "output"] {
        std::fs::write(home.join(format!("hello.{role}.schema.json")), schema).expect("schema");
    }
    std::fs::write(home.join("input.json"), r#"{"text":"hello"}"#).expect("input");
    success(fixture.run(&home, &["check", "hello.atom.astro"], ""));
    let prompt = success(fixture.run(
        &home,
        &["render", "hello.atom.astro", "--input", "input.json"],
        "",
    ));
    assert!(prompt.contains("local component") && !prompt.contains("never-read-this"));
    #[cfg(unix)]
    {
        std::fs::rename(home.join("Rules.astro"), home.join("ActualRules.astro")).expect("rename");
        std::os::unix::fs::symlink(home.join("ActualRules.astro"), home.join("Rules.astro"))
            .expect("contained alias");
        std::os::unix::fs::symlink(home.join("ActualRules.astro"), home.join("Shared.astro"))
            .expect("second alias");
        let atom_file = home.join("hello.atom.astro");
        let source = std::fs::read_to_string(&atom_file).expect("source");
        std::fs::write(
            &atom_file,
            source
                .replace(
                    "import Rules",
                    "import Shared from './Shared.astro';\nimport Rules",
                )
                .replace("<Rules />", "<Rules /><Shared />"),
        )
        .expect("alias imports");
        success(fixture.run(&home, &["check", "hello.atom.astro"], ""));
        std::fs::remove_file(home.join("Rules.astro")).expect("remove");
        std::os::unix::fs::symlink(home.join("unrelated-secret"), home.join("Rules.astro"))
            .expect("symlink");
        let output = fixture.run(&home, &["check", "hello.atom.astro"], "");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be an Astro source"));
    }
}

#[test]
fn a_contextual_atom_in_the_home_directory_keeps_its_state_beneath_home() {
    let fixture = Fixture::new("home-context");
    let home = fixture.0.join("home");
    let example = repository().join("examples/12-execution-context");
    std::fs::create_dir_all(&home).expect("home");
    for entry in std::fs::read_dir(example.join("atoms")).expect("atoms") {
        let entry = entry.expect("entry");
        let text = std::fs::read_to_string(entry.path()).expect("read");
        std::fs::write(
            home.join(entry.file_name()),
            text.replace("'../.atomic/types/", "'./.atomic/types/"),
        )
        .expect("write");
    }
    std::fs::copy(
        example.join("context.settings.json"),
        home.join("context.settings.json"),
    )
    .expect("settings");
    std::fs::copy(
        example.join("fixtures/context-reply/ordinary.input.json"),
        home.join("input.json"),
    )
    .expect("input");
    success(fixture.run(&home, &["check", "context-reply.atom.astro"], ""));
    let args = [
        "render",
        "context-reply.atom.astro",
        "--input",
        "input.json",
        "--context-settings",
        "context.settings.json",
        "--run-id",
        "home-run",
    ];
    let first = success(fixture.run(&home, &args, "home-region"));
    assert!(first.contains("home-region"));
    let restored = success(fixture.run(&home, &args, "other-region"));
    assert!(restored.contains("home-region") && !restored.contains("other-region"));
    // The compiler cache and the context key follow ATOMIC_HOME.
    let atomic_home = home.join(".local/share/atomic");
    assert!(atomic_home.join("cache/compiler").is_dir());
    assert!(atomic_home.join("keys/context.key").is_file());
    assert!(!home.join(".cache/atomic").exists());
}

#[cfg(unix)]
#[test]
fn generated_declarations_never_follow_a_symlink_out_of_the_project() {
    let fixture = Fixture::new("declaration-symlink");
    let project = fixture.0.join("project");
    copy_tree(&repository().join("examples/01-classify"), &project);
    let outside = fixture.0.join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::create_dir_all(project.join(".atomic")).expect(".atomic");
    std::os::unix::fs::symlink(&outside, project.join(".atomic/types")).expect("symlink");
    let output = fixture.run(&project, &["check"], "");
    assert!(
        !output.status.success(),
        "a symlinked types directory is refused"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("escapes project"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0);
}

#[test]
fn two_atoms_with_one_file_name_are_refused() {
    // Generated declarations are named after the file, so one project cannot
    // hold two atoms with one file name, even when their `name`s differ.
    let fixture = Fixture::new("declaration-collision");
    let project = fixture.0.join("project");
    copy_tree(&repository().join("examples/01-classify"), &project);
    let nested = project.join("atoms/again");
    std::fs::create_dir_all(&nested).expect("nested");
    for entry in std::fs::read_dir(project.join("atoms")).expect("atoms") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            let text = std::fs::read_to_string(entry.path()).expect("read");
            std::fs::write(
                nested.join(entry.file_name()),
                text.replace("'../.atomic/types/", "'../../.atomic/types/")
                    .replace(
                        "\"description\":",
                        "\"name\": \"classify-note-again\",\n  \"description\":",
                    ),
            )
            .expect("write");
        }
    }
    let output = fixture.run(&project, &["check"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("are named \"classify-note\""), "{stderr}");
}

#[test]
fn a_misspelled_atom_reference_fails_atomic_check() {
    let fixture = Fixture::new("misspelled-atom");
    let project = fixture.0.join("project");
    copy_tree(&repository().join("examples/02-change-request"), &project);
    let file = project.join("workflows/change-request.workflow.yaml");
    let text = std::fs::read_to_string(&file).expect("workflow");
    std::fs::write(&file, text.replace("atom: plan-task", "atom: plan-taks")).expect("write");
    let output = fixture.run(&project, &["check"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("change-request.workflow.yaml")
            && stderr.contains("/steps/plan/atom")
            && stderr.contains("did you mean plan-task?"),
        "{stderr}"
    );
}

#[test]
fn an_unlowerable_schema_fails_atomic_check_with_its_file_and_path() {
    let fixture = Fixture::new("unlowerable");
    let project = fixture.0.join("project");
    copy_tree(&repository().join("examples/01-classify"), &project);
    // A map with free-form keys has no strict-mode form.
    let file = project.join("atoms/classify-note.output.schema.yaml");
    let text = std::fs::read_to_string(&file).expect("schema");
    std::fs::write(
        &file,
        text.replace(
            "properties:\n",
            "properties:\n  labels:\n    type: object\n    additionalProperties: {type: string}\n",
        ),
    )
    .expect("write");
    let output = fixture.run(&project, &["check"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("atoms/classify-note.output.schema.yaml")
            && stderr.contains("/properties/labels")
            && stderr.contains("cannot be lowered"),
        "{stderr}"
    );
}
