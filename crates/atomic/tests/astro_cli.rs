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

fn schema_prune_project(project: &Path, change_module: &str) {
    std::fs::create_dir_all(project.join("atoms")).expect("atoms");
    std::fs::create_dir_all(project.join("schemas")).expect("schemas");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: schema-prune\n",
    )
    .expect("manifest");
    std::fs::write(
        project.join("schemas/tier.schema.yaml"),
        "title: Tier\nenum: [tier-1, tier-2]\n",
    )
    .expect("tier");
    std::fs::write(
        project.join(format!("schemas/{change_module}.schema.yaml")),
        "title: Change\ntype: object\nrequired: [id]\nproperties:\n  id: {type: string}\n",
    )
    .expect("change");
    std::fs::write(
        project.join("atoms/show.atom.astro"),
        format!(
            "---\n\
             import type {{ Props, Input, Output }} from '../.atomic/types/show';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             import type {{ Change }} from '../.atomic/types/schemas/{change_module}';\n\
             import type {{ Tier }} from '../.atomic/types/schemas/tier';\n\
             const {{ input }}: Props = Astro.props;\n\
             const change: Change = {{ id: 'CHG-1' }};\n\
             const tier: Tier = 'tier-1';\n\
             export const config: AtomConfig<Input, Output> = {{}};\n\
             ---\n\
             Show {{input.text}}: {{change.id}} ({{tier}}).\n"
        ),
    )
    .expect("atom");
    for (role, schema) in [
        (
            "input",
            "type: object\nrequired: [text]\nproperties:\n  text: {type: string}\n",
        ),
        (
            "output",
            "type: object\nrequired: [summary]\nproperties:\n  summary: {type: string}\n",
        ),
    ] {
        std::fs::write(
            project.join(format!("atoms/show.{role}.schema.yaml")),
            schema,
        )
        .expect("sidecar");
    }
}

#[test]
fn project_check_prunes_renamed_shared_schema_modules_after_validating() {
    let fixture = Fixture::new("schema-prune");
    let project = fixture.0.join("project");
    schema_prune_project(&project, "change");
    let types = project.join(".atomic/types/schemas");
    success(fixture.run(&project, &["check"], ""));
    assert!(types.join("change.d.ts").is_file());
    assert!(types.join("tier.d.ts").is_file());
    std::fs::write(types.join("manual.d.ts"), "export type Manual = string;\n")
        .expect("user declaration");
    std::fs::write(types.join("unreadable.d.ts"), [0xff, 0xfe])
        .expect("user declaration without UTF-8 text");
    std::fs::write(
        types.join("lookalike.d.ts"),
        "// Generated by atomic. Do not edit.\n// @atomic-schema schemas/change.schema.yaml\nexport type Lookalike = string;\n",
    )
    .expect("mismatched declaration");

    std::fs::rename(
        project.join("schemas/change.schema.yaml"),
        project.join("schemas/finding.schema.yaml"),
    )
    .expect("rename schema");
    let stale_use = fixture.run(&project, &["check"], "");
    assert!(
        !stale_use.status.success(),
        "a stale import passed checking"
    );
    assert!(
        types.join("change.d.ts").is_file(),
        "a failed check pruned output"
    );

    // The type name comes from `title`, so only the direct import path changes.
    let atom = project.join("atoms/show.atom.astro");
    let old = std::fs::read_to_string(&atom).expect("atom source");
    std::fs::write(&atom, old.replace("schemas/change", "schemas/finding")).expect("update import");
    success(fixture.run(&project, &["check"], ""));
    assert!(!types.join("change.d.ts").exists(), "stale module survived");
    assert!(
        types.join("finding.d.ts").is_file(),
        "renamed module missing"
    );
    assert!(types.join("tier.d.ts").is_file(), "live module was pruned");
    assert!(
        types.join("manual.d.ts").is_file(),
        "user declaration was pruned"
    );
    assert!(
        types.join("unreadable.d.ts").is_file(),
        "unreadable declaration was pruned"
    );
    assert!(
        types.join("lookalike.d.ts").is_file(),
        "mismatched declaration was pruned"
    );
    success(fixture.run(&project, &["check"], ""));
    assert!(
        !types.join("change.d.ts").exists(),
        "second check regrew stale module"
    );
}

#[test]
fn project_check_prunes_root_level_schema_modules_after_rename_and_removal() {
    let fixture = Fixture::new("schema-prune-root");
    let project = fixture.0.join("project");
    schema_prune_project(&project, "change");
    std::fs::rename(
        project.join("schemas/change.schema.yaml"),
        project.join("change.schema.yaml"),
    )
    .expect("move schema to project root");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: schema-prune\ncollections:\n  schemas: ['*.schema.yaml', 'schemas/**/*.schema.yaml']\n",
    )
    .expect("root-level schema collection");
    let atom = project.join("atoms/show.atom.astro");
    let original = std::fs::read_to_string(&atom).expect("atom source");
    std::fs::write(&atom, original.replace("schemas/change", "change")).expect("root-level import");

    let types = project.join(".atomic/types");
    success(fixture.run(&project, &["check"], ""));
    assert!(
        types.join("change.d.ts").is_file(),
        "root-level module missing"
    );
    assert!(
        types.join("schemas/tier.d.ts").is_file(),
        "nested module missing"
    );
    std::fs::write(
        types.join("manual-root.d.ts"),
        "export type ManualRoot = string;\n",
    )
    .expect("user declaration");

    std::fs::rename(
        project.join("change.schema.yaml"),
        project.join("finding.schema.yaml"),
    )
    .expect("rename root schema");
    let stale_use = fixture.run(&project, &["check"], "");
    assert!(
        !stale_use.status.success(),
        "old root-level import passed checking"
    );
    let source = std::fs::read_to_string(&atom).expect("atom source");
    std::fs::write(&atom, source.replace("types/change", "types/finding")).expect("renamed import");
    success(fixture.run(&project, &["check"], ""));
    assert!(
        !types.join("change.d.ts").exists(),
        "old root module survived"
    );
    assert!(
        types.join("finding.d.ts").is_file(),
        "new root module missing"
    );
    assert!(
        types.join("manual-root.d.ts").is_file(),
        "user root module was pruned"
    );

    std::fs::remove_file(&atom).expect("remove atom");
    std::fs::remove_file(project.join("finding.schema.yaml")).expect("remove schema");
    success(fixture.run(&project, &["check"], ""));
    assert!(
        !types.join("finding.d.ts").exists(),
        "removed schema module survived"
    );
    assert!(
        !types.join("schemas/tier.d.ts").exists(),
        "orphaned nested module survived"
    );
    assert!(
        types.join("manual-root.d.ts").is_file(),
        "user root module was pruned"
    );
}

#[cfg(unix)]
#[test]
fn project_check_refuses_a_link_in_the_generated_schema_tree() {
    let fixture = Fixture::new("schema-prune-link");
    let project = fixture.0.join("project");
    std::fs::create_dir_all(project.join(".atomic/types/schemas")).expect("generated tree");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: schema-prune-link\n",
    )
    .expect("manifest");
    let outside = fixture.0.join("outside.d.ts");
    std::fs::write(&outside, "outside stays\n").expect("outside");
    std::os::unix::fs::symlink(&outside, project.join(".atomic/types/schemas/old.d.ts"))
        .expect("link");

    let output = fixture.run(&project, &["check"], "");
    assert!(
        !output.status.success(),
        "check followed a generated-tree link"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("generated schema tree contains a symlink"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside"),
        "outside stays\n"
    );
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
fn check_project_flag_selects_a_project_without_a_positional_path() {
    let fixture = Fixture::new("check-project-flag");
    let project = fixture.0.join("project");
    let outside = fixture.0.join("outside");
    copy_tree(&repository().join("examples/01-classify"), &project);
    std::fs::create_dir_all(&outside).expect("outside directory");

    let manifest = project.join("atomic.yaml");
    for named in [&project, &manifest] {
        let checked = fixture.run(
            &outside,
            &[
                "--json",
                "check",
                "--project",
                named.to_str().expect("project path"),
            ],
            "",
        );
        let checked: serde_json::Value =
            serde_json::from_str(&success(checked)).expect("check JSON");
        assert_eq!(checked["checked"]["atoms"], 1);
        assert_eq!(checked["checked"]["workflows"], 0);
    }

    let missing = outside.join("missing-project");
    let refused = fixture.run(
        &outside,
        &[
            "--json",
            "check",
            "--project",
            missing.to_str().expect("missing project path"),
        ],
        "",
    );
    assert!(!refused.status.success(), "missing project was checked");
    let refusal: serde_json::Value = serde_json::from_slice(&refused.stdout).expect("refusal JSON");
    assert_eq!(refusal["error"]["kind"], "project.not_found");
    assert!(
        refusal["error"]["description"]
            .as_str()
            .expect("refusal description")
            .contains(missing.to_str().expect("missing project path")),
        "{refusal}"
    );
}

#[test]
fn repeated_source_path_renders_each_distinct_value_with_its_own_binding() {
    let fixture = Fixture::new("repeated-binding");
    let project = fixture.0.join("project");
    let atoms = project.join("atoms");
    std::fs::create_dir_all(&atoms).expect("atoms");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: repeated-bindings\n",
    )
    .expect("manifest");
    std::fs::write(
        atoms.join("group.atom.astro"),
        "---\nimport type { Props } from '../.atomic/types/group';\nconst { input }: Props = Astro.props;\n---\n{input.groups.map((group) => <Fragment>{input.items.filter((item) => item.group === group)}</Fragment>)}\n",
    )
    .expect("atom");
    std::fs::write(
        atoms.join("group.input.schema.json"),
        r#"{"type":"object","additionalProperties":false,"required":["groups","items"],"properties":{"groups":{"type":"array","maxItems":3,"items":{"type":"string","maxLength":20}},"items":{"type":"array","maxItems":2,"items":{"type":"object","additionalProperties":false,"required":["group","message"],"properties":{"group":{"type":"string","maxLength":20},"message":{"type":"string","maxLength":20}}}}}}"#,
    )
    .expect("input schema");
    std::fs::write(
        atoms.join("group.output.schema.json"),
        r#"{"type":"object","additionalProperties":false,"required":["summary"],"properties":{"summary":{"type":"string","maxLength":100}}}"#,
    )
    .expect("output schema");
    std::fs::write(
        project.join("input.json"),
        r#"{"groups":["first","second","first"],"items":[{"group":"first","message":"one"},{"group":"second","message":"two"}]}"#,
    )
    .expect("input");
    success(fixture.run(&project, &["check"], ""));
    let output = success(fixture.run(
        &project,
        &["render", "atoms/group.atom.astro", "--input", "input.json"],
        "",
    ));
    let prompt: serde_json::Value = serde_json::from_str(&output).expect("prompt JSON");
    let user = prompt["user"].as_str().expect("user turn");
    let developer = prompt["developer"].as_str().expect("developer turn");
    assert!(user.contains("\"message\":\"one\""));
    assert!(user.contains("\"message\":\"two\""));
    assert_eq!(
        user.matches("path=").count(),
        2,
        "both distinct values have user bindings; the exact repeated value is shared"
    );
    assert_eq!(user.matches("origin=\"external\"").count(), 2);
    let references: Vec<_> = developer
        .split("see <data-")
        .skip(1)
        .map(|part| part.split('>').next().expect("wrapper suffix"))
        .collect();
    assert_eq!(references.len(), 3);
    assert_ne!(
        references[0], references[1],
        "distinct values need distinct wrappers"
    );
    assert_eq!(
        references[0], references[2],
        "an exact repeat shares its wrapper"
    );
}

#[test]
fn previews_follow_edits_while_explicit_runs_keep_their_checkpoint() {
    let fixture = Fixture::new("preview");
    let project = fixture.0.join("project");
    copy_tree(
        &repository().join("examples/12-execution-context"),
        &project,
    );
    // The CLI profile selects a project layer as well as the explicit
    // context-settings profile used by this preview fixture.
    std::fs::write(project.join("atomic.test.yaml"), "{}\n").expect("profile");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("an imported source must be an Astro component, a .ts helper or a literal Markdown file inside the project root"),
            "{stderr}"
        );
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
    // The context key follows ATOMIC_HOME; the compiler, embedded in the
    // binary, unpacks nothing.
    let atomic_home = home.join(".local/share/atomic");
    assert!(atomic_home.join("keys/context.key").is_file());
    assert!(!atomic_home.join("cache/compiler").exists());
    assert!(!home.join(".cache/atomic").exists());
}

#[test]
fn a_per_atom_config_atom_is_checked_run_and_refused_by_the_cli() {
    // Two atoms written with the `Config` their own declarations generate, in
    // a directory of their own: one declares a public context that only a
    // validator reads, one declares none.
    let fixture = Fixture::new("per-atom-config");
    let home = fixture.0.join("home");
    std::fs::create_dir_all(&home).expect("fixture");
    let context = "{type:'object',required:['profile','config'],properties:{profile:{type:'string'},\
        config:{type:'object',required:['maxReplyCharacters'],properties:{maxReplyCharacters:{type:'integer'}}}}}";
    let validator = "validate:[({output, context}) => output.ticket.length <= context.config.maxReplyCharacters]";
    std::fs::write(
        home.join("reply.atom.astro"),
        format!(
            "---\nimport type {{ Props, Config }} from './.atomic/types/reply';\nconst {{ input }}: Props = Astro.props;\nexport const config: Config = {{context:{context},{validator}}};\n---\nDraft a reply for: {{input.text}}\n"
        ),
    )
    .expect("atom");
    std::fs::write(
        home.join("plain.atom.astro"),
        "---\nimport type { Props, Config } from './.atomic/types/plain';\nconst { input }: Props = Astro.props;\nexport const config: Config = {validate:[({output}) => output.ticket.length > 0]};\n---\nNote: {input.text}\n",
    )
    .expect("atom");
    for name in ["reply", "plain"] {
        std::fs::write(
            home.join(format!("{name}.input.schema.json")),
            r#"{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string"}}}"#,
        )
        .expect("input schema");
        std::fs::write(
            home.join(format!("{name}.output.schema.json")),
            r#"{"type":"object","additionalProperties":false,"required":["ticket"],"properties":{"ticket":{"type":"string"}}}"#,
        )
        .expect("output schema");
    }
    std::fs::write(
        home.join("input.json"),
        r#"{"text":"the door lock is stuck"}"#,
    )
    .expect("input");
    std::fs::copy(
        repository().join("examples/12-execution-context/context.settings.json"),
        home.join("context.settings.json"),
    )
    .expect("settings");
    // Both check, and the atom with no declared context runs with no settings.
    success(fixture.run(&home, &["check", "reply.atom.astro"], "cli-region"));
    success(fixture.run(&home, &["check", "plain.atom.astro"], "cli-region"));
    let prompt = success(fixture.run(
        &home,
        &["render", "plain.atom.astro", "--input", "input.json"],
        "cli-region",
    ));
    assert!(prompt.contains("door lock"), "{prompt}");
    // The contextual atom is refused before it renders anything, because the
    // context it declares has come from nowhere.
    let refused = fixture.run(
        &home,
        &["render", "reply.atom.astro", "--input", "input.json"],
        "cli-region",
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(stderr.contains("--context-settings"), "{stderr}");
    let prompt = success(fixture.run(
        &home,
        &[
            "render",
            "reply.atom.astro",
            "--input",
            "input.json",
            "--context-settings",
            "context.settings.json",
            "--run-id",
            "per-atom-run",
        ],
        "cli-region",
    ));
    assert!(prompt.contains("door lock"), "{prompt}");
    // A misspelled field of the output is refused by `atomic check`, against the
    // same generated declarations stock Astro editor support reads
    // (docs/reference/atom-files.md: "Stock Astro VS Code support checks the
    // generated `Props` binding").
    let atom = home.join("reply.atom.astro");
    std::fs::write(
        &atom,
        std::fs::read_to_string(&atom)
            .expect("source")
            .replace("output.ticket.length", "output.tickett.length"),
    )
    .expect("source");
    let output = fixture.run(&home, &["check", "reply.atom.astro"], "cli-region");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("tickett"), "{stderr}");
    assert!(
        stderr.contains("TS2339") || stderr.contains("TS2551"),
        "{stderr}"
    );
    // And a `context` read by an atom that declares none is refused the same
    // way, so no run reaches a callback with `context` simply absent.
    std::fs::write(
        home.join("plain.atom.astro"),
        "---\nimport type { Props, Config } from './.atomic/types/plain';\nconst { input }: Props = Astro.props;\nexport const config: Config = {validate:[({output, context}) => context.profile === 'test' && output.ticket.length > 0]};\n---\nNote: {input.text}\n",
    )
    .expect("atom");
    let output = fixture.run(&home, &["check", "plain.atom.astro"], "cli-region");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("context"), "{stderr}");
    assert!(
        stderr.contains("TS2339") || stderr.contains("TS2551"),
        "{stderr}"
    );
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

/// A project as `docs/guides/vs-code.md` describes it: an atom, a prompt
/// component that imports a shared schema type, and the schema files that
/// type and the field it points at live in.
fn shared_type_project(project: &Path) {
    // A `\` at the end of a line swallows the next line's indent, so the YAML
    // and Astro sources below write their leading spaces as `\x20`.
    std::fs::create_dir_all(project.join("atoms")).expect("atoms");
    std::fs::create_dir_all(project.join("schemas")).expect("schemas");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: shared-types\n",
    )
    .expect("manifest");
    std::fs::write(
        project.join("schemas/tier.schema.yaml"),
        "title: Tier\nenum: [tier-1, tier-2]\n",
    )
    .expect("tier");
    std::fs::write(
        project.join("schemas/change.schema.yaml"),
        "type: object\n\
         additionalProperties: false\n\
         required: [id, tier]\n\
         properties:\n\
         \x20 id: {type: string, description: The change ID.}\n\
         \x20 tier: {$ref: tier.schema.yaml}\n",
    )
    .expect("change");
    std::fs::write(
        project.join("atoms/assess.atom.astro"),
        "---\n\
         import type { Props, Input, Output } from '../.atomic/types/assess';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         import type { Change } from '../.atomic/types/schemas/change';\n\
         import Summary from './Summary.astro';\n\
         type Shown = Pick<Change, 'id'>;\n\
         const { input }: Props = Astro.props;\n\
         const shown: Shown = { id: 'CHG-1' };\n\
         export const config: AtomConfig<Input, Output> = {};\n\
         ---\n\
         Note {input.text} about {shown.id}.\n\
         <Summary change={{ id: shown.id, tier: 'tier-1' }} />\n",
    )
    .expect("atom");
    std::fs::write(
        project.join("atoms/Summary.astro"),
        "---\n\
         import type { Change } from '../.atomic/types/schemas/change';\n\
         interface Props { change: Change }\n\
         const { change }: Props = Astro.props;\n\
         ---\n\
         Change {change.id} ({change.tier})\n",
    )
    .expect("component");
    for (role, schema) in [
        (
            "input",
            "type: object\nadditionalProperties: false\nrequired: [text]\nproperties:\n\x20 text: {type: string}\n",
        ),
        (
            "output",
            "type: object\nadditionalProperties: false\nrequired: [summary]\nproperties:\n\x20 summary: {type: string}\n",
        ),
    ] {
        std::fs::write(
            project.join(format!("atoms/assess.{role}.schema.yaml")),
            schema,
        )
        .expect("sidecar");
    }
}

#[test]
fn check_writes_the_schema_origin_of_every_generated_type_and_field() {
    let fixture = Fixture::new("schema-origins");
    let project = fixture.0.join("project");
    shared_type_project(&project);
    let types = project.join(".atomic/types");
    success(fixture.run(&project, &["check"], ""));
    let change = std::fs::read_to_string(types.join("schemas/change.d.ts")).expect("change module");
    // What a person sees when they hover the field in the prompt component:
    // the schema file to open and the pointer to look at. `tier` is a `$ref`,
    // so its origin is the other file. These two modules are the whole of what
    // `docs/guides/vs-code.md` shows, so the guide and the generator say one
    // thing.
    assert_eq!(
        change,
        "// Generated by atomic. Do not edit.\n\
         // @atomic-schema schemas/change.schema.yaml\n\
         import type { Tier } from './tier';\n\
         /** @atomic-schema schemas/change.schema.yaml# */\n\
         export type Change = { /** The change ID. @atomic-schema schemas/change.schema.yaml#/properties/id */ \"id\": string; /** @atomic-schema schemas/tier.schema.yaml# */ \"tier\": Tier; };\n"
    );
    let tier = std::fs::read_to_string(types.join("schemas/tier.d.ts")).expect("tier module");
    assert_eq!(
        tier,
        "// Generated by atomic. Do not edit.\n\
         // @atomic-schema schemas/tier.schema.yaml\n\
         /** @atomic-schema schemas/tier.schema.yaml# */\n\
         export type Tier = \"tier-1\" | \"tier-2\";\n"
    );
    // The atom's own declarations come from its contracts, not one schema
    // file, so they name no schema file.
    let own = std::fs::read_to_string(types.join("assess.d.ts")).expect("atom module");
    assert!(!own.contains("@atomic-schema"), "{own}");
}

#[test]
fn a_renamed_shared_schema_fails_atomic_check_naming_the_file_it_looked_for() {
    let fixture = Fixture::new("schema-rename");
    let project = fixture.0.join("project");
    shared_type_project(&project);
    success(fixture.run(&project, &["check"], ""));
    std::fs::rename(
        project.join("schemas/change.schema.yaml"),
        project.join("schemas/finding.schema.yaml"),
    )
    .expect("rename");
    let output = fixture.run(&project, &["check"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    // The rename is diagnosed by name, in the file that holds the import, so
    // the person editing sees which schema file to write back or point at.
    assert!(
        stderr.contains("schemas/change.schema.json")
            && stderr.contains("schemas/change.schema.yaml")
            && stderr.contains(".atomic/types/schemas/change"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Summary.astro") || stderr.contains("assess.atom.astro"),
        "{stderr}"
    );
    // The schema file under its new name still checks on its own.
    std::fs::write(
        project.join("atoms/Summary.astro"),
        "---\nimport type { Finding } from '../.atomic/types/schemas/finding';\n\
         interface Props { finding: Finding }\nconst { finding }: Props = Astro.props;\n---\n\
         Finding {finding.id}\n",
    )
    .expect("component");
    std::fs::write(
        project.join("atoms/assess.atom.astro"),
        "---\n\
         import type { Props, Input, Output } from '../.atomic/types/assess';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         import Summary from './Summary.astro';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = {};\n\
         ---\n\
         Note {input.text}.\n\
         <Summary finding={{ id: 'CHG-1', tier: 'tier-1' }} />\n",
    )
    .expect("atom");
    success(fixture.run(&project, &["check"], ""));
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
