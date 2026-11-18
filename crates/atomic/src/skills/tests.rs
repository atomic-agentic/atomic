// SPDX-License-Identifier: Apache-2.0

use super::*;
use atomic_core::{atom, compile};
use atomic_schema::registry::Registry;
use serde_json::json;
use std::path::{Path, PathBuf};

/// The input and output schemas the syntax examples are written against.
const EXAMPLE_INPUT: &str = r#"{"type":"object","additionalProperties":false,"required":["text","items","urgent"],"properties":{"text":{"type":"string","maxLength":4000},"items":{"type":"array","maxItems":20,"items":{"type":"string","maxLength":200}},"urgent":{"type":"boolean"},"extra":{"type":"object","additionalProperties":false,"required":["note"],"properties":{"note":{"type":"string","maxLength":200}}}}}"#;
/// See [`EXAMPLE_INPUT`].
const EXAMPLE_OUTPUT: &str = r#"{"type":"object","additionalProperties":false,"required":["summary","labels"],"properties":{"summary":{"type":"string","minLength":1,"maxLength":200},"labels":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":30}}}}"#;
/// The input the [`CEL_EXAMPLES`] are evaluated with.
const CEL_INPUT: &str =
    r#"{"text":"Export crashes when the file name contains an accented character."}"#;
/// The output the [`CEL_EXAMPLES`] are evaluated with.
const CEL_OUTPUT: &str = r#"{"category":"bug","summary":"Export crashes on accented file names.","labels":["export","crash"],"priority":2}"#;
/// The input schema the [`CEL_EXAMPLES`] are checked against.
const CEL_INPUT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string","maxLength":4000}}}"#;
/// The output schema the [`CEL_EXAMPLES`] are checked against.
const CEL_OUTPUT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["category","summary","labels","priority"],"properties":{"category":{"type":"string","enum":["bug","feature","question"]},"summary":{"type":"string","maxLength":160},"labels":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":30}},"priority":{"type":"integer","minimum":1,"maximum":3},"duplicate_of":{"type":"string","maxLength":40}}}"#;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-skills-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The repository's copy of the pack matches what this binary installs.
/// `ATOMIC_TEST_BLESS=1` rewrites the copy instead.
#[test]
fn checked_in_pack_matches_the_binary() {
    let root = repository().join("skills");
    let bless = std::env::var_os("ATOMIC_TEST_BLESS").is_some();
    let pack = pack().expect("the pack generates");
    let mut stale = Vec::new();
    for file in &pack {
        let path = root.join(&file.path);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(file.contents.as_str()) {
            if bless {
                std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                std::fs::write(&path, &file.contents).expect("bless");
            } else {
                stale.push(file.path.clone());
            }
        }
    }
    assert!(
        stale.is_empty(),
        "skills/ differs from what this build generates: {stale:?}; run \
         `ATOMIC_TEST_BLESS=1 scripts/dev.sh cargo test -p atomic-agentic skills` and commit the result"
    );
    // Nothing else is in the skill's directory, so the copy is the whole pack.
    let mut present = Vec::new();
    let mut dirs = vec![root.join(CREATE_ATOM)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read skills/") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let relative = path.strip_prefix(&root).expect("inside");
                present.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    present.sort();
    let mut expected: Vec<_> = pack.iter().map(|f| f.path.clone()).collect();
    expected.sort();
    assert_eq!(
        present, expected,
        "files in skills/ that the pack does not have"
    );
}

#[test]
fn a_frontmatter_change_changes_the_generated_reference() {
    let binary = Definitions::binary();
    let before = fields_reference(&binary).expect("generates");
    assert!(before.contains("- **`sandbox`** (`'read-only' | 'workspace-write'`)"));

    // A changed type in the declaration the compiler checks against.
    let declarations = binary.declarations.replace(
        "sandbox?: 'read-only' | 'workspace-write';",
        "sandbox?: 'read-only';",
    );
    assert_ne!(
        declarations, binary.declarations,
        "the test edits a real line"
    );
    let changed = fields_reference(&Definitions {
        declarations: &declarations,
        ..binary
    })
    .expect("generates");
    assert!(
        changed.contains("- **`sandbox`** (`'read-only'`)"),
        "{changed}"
    );

    // A kind this version stops running is reported as refused.
    let kinds: Vec<&str> = binary
        .kinds
        .iter()
        .copied()
        .filter(|k| *k != "exec")
        .collect();
    let changed = fields_reference(&Definitions {
        kinds: &kinds,
        ..binary
    })
    .expect("generates");
    assert!(changed.contains("also allows `exec`, `a2a`"), "{changed}");

    // A field the loader starts refusing moves to the refused list.
    let fields: Vec<&str> = binary
        .fields
        .iter()
        .copied()
        .filter(|f| *f != "trust")
        .collect();
    let mut later = binary.later.to_vec();
    later.push("trust");
    let changed = fields_reference(&Definitions {
        fields: &fields,
        later: &later,
        ..binary
    });
    // Its note is now stale, which fails generation until it is removed.
    assert_eq!(
        changed.expect_err("stale note"),
        "a note describes `trust`, which the loader does not read"
    );
}

#[test]
fn a_new_field_without_a_note_or_a_type_fails_generation() {
    let binary = Definitions::binary();
    let mut fields = binary.fields.to_vec();
    fields.push("colour");
    let error = fields_reference(&Definitions {
        fields: &fields,
        ..binary
    })
    .expect_err("no note");
    assert!(error.contains("`colour`"), "{error}");

    let declarations = binary
        .declarations
        .replace("  sandbox?:", "  colour?: string;\n  sandbox?:");
    let error = fields_reference(&Definitions {
        declarations: &declarations,
        ..binary
    })
    .expect_err("undeclared in the loader");
    assert!(error.contains("`colour`"), "{error}");
}

#[test]
fn the_declaration_reader_refuses_lines_it_does_not_understand() {
    let members = config_members(atom::AUTHORING_DECLARATIONS).expect("reads");
    for field in atom::FIELDS {
        assert!(members.iter().any(|m| m.name == *field), "{field}");
    }
    let odd = atom::AUTHORING_DECLARATIONS.replace("  name?: string;", "  name?: string");
    assert!(config_members(&odd).is_err());
    assert!(config_members("export interface Other {}").is_err());
}

/// Compile a construct in a scratch project, returning the check's error.
fn compile_construct(c: &Construct) -> Result<(), String> {
    let dir = scratch("construct");
    let source = format!(
        "---\nimport type {{ Props, Input, Output }} from './.atomic/types/test';\n\
         import type {{ AtomConfig }} from './.atomic/types/authoring';\n{}\n\
         const {{ input }}: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = {};\n---\n{}\n",
        c.frontmatter, c.config, c.body
    );
    std::fs::write(dir.join("test.atom.astro"), source).expect("atom");
    std::fs::write(dir.join("test.input.schema.json"), EXAMPLE_INPUT).expect("input");
    std::fs::write(dir.join("test.output.schema.json"), EXAMPLE_OUTPUT).expect("output");
    if let Some((file, source)) = c.component {
        std::fs::write(dir.join(file), source).expect("component");
    }
    let mut registry = Registry::new();
    let atom =
        atom::load(&dir.join("test.atom.astro"), &mut registry).map_err(|e| e.to_string())?;
    compile::compile(&atom, &registry).map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
fn every_construct_in_the_reference_compiles() {
    for c in CONSTRUCTS {
        compile_construct(c).unwrap_or_else(|e| panic!("{}: {e}", c.title));
    }
}

#[test]
fn every_mistake_in_the_reference_is_refused() {
    for m in MISTAKES {
        let error = compile_construct(&m.wrong).expect_err(m.title);
        assert!(error.contains(m.says), "{}: {error}", m.title);
    }
}

#[test]
fn every_cel_example_checks_against_the_schemas_and_holds() {
    let input_schema: Value = serde_json::from_str(CEL_INPUT_SCHEMA).expect("schema");
    let output_schema: Value = serde_json::from_str(CEL_OUTPUT_SCHEMA).expect("schema");
    let input: Value = serde_json::from_str(CEL_INPUT).expect("input");
    let output: Value = serde_json::from_str(CEL_OUTPUT).expect("output");
    for (role, schema, value) in [
        ("input", &input_schema, &input),
        ("output", &output_schema, &output),
    ] {
        assert!(
            jsonschema_valid(schema, value),
            "the cheat sheet's {role} matches its schema"
        );
    }
    let scope = atomic_schema::expr::Scope::new()
        .with("input", &input_schema)
        .with("output", &output_schema);
    for (source, meaning) in CEL_EXAMPLES {
        let expression = atomic_schema::expr::parse(source).expect(source);
        expression
            .check(&scope)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(
            evaluate(source, &input, &output),
            cel::Value::Bool(true),
            "{meaning}: {source}"
        );
    }
    // The checker refuses a misspelt field rather than evaluating it.
    let misspelt = atomic_schema::expr::parse("output.sumary.size() > 0").expect("parses");
    assert!(misspelt.check(&scope).is_err());
}

fn evaluate(source: &str, input: &Value, output: &Value) -> cel::Value {
    let program = cel::Program::compile(source).expect(source);
    let mut context = cel::Context::default();
    context.add_variable_from_value("input", cel_value(input));
    context.add_variable_from_value("output", cel_value(output));
    program.execute(&context).expect(source)
}

/// JSON as CEL values. Not through serde: with serde_json's
/// `arbitrary_precision`, which the dependency graph enables, a number
/// serializes as a map.
fn cel_value(value: &Value) -> cel::Value {
    match value {
        Value::Null => cel::Value::Null,
        Value::Bool(b) => cel::Value::Bool(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => cel::Value::Int(i),
            (None, Some(u), _) => cel::Value::UInt(u),
            (None, None, f) => cel::Value::Float(f.expect("a finite number")),
        },
        Value::String(s) => cel::Value::String(std::sync::Arc::new(s.clone())),
        Value::Array(items) => {
            cel::Value::List(std::sync::Arc::new(items.iter().map(cel_value).collect()))
        }
        Value::Object(fields) => cel::Value::from(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), cel_value(v)))
                .collect::<std::collections::HashMap<String, cel::Value>>(),
        ),
    }
}

fn jsonschema_valid(schema: &Value, value: &Value) -> bool {
    let mut registry = Registry::new();
    let id = registry
        .register(PathBuf::from("/cheat-sheet.schema.json"), schema.clone())
        .expect("registers");
    registry
        .validator(&id)
        .expect("validator")
        .validate(value)
        .is_ok()
}

#[test]
fn the_strict_rules_match_the_lowering() {
    use atomic_schema::lower::{OPENAI_STRICT, lower};
    let text = strict_reference(&OPENAI_STRICT);
    for keyword in OPENAI_STRICT.refused {
        assert!(text.contains(&format!("`{keyword}`")), "{keyword}");
        let mut refused = serde_json::Map::new();
        refused.insert((*keyword).to_owned(), json!([]));
        let schema =
            json!({"type":"object","additionalProperties":false,"properties":{"x":refused}});
        assert!(lower(&schema, &OPENAI_STRICT).is_err(), "{keyword} lowers");
    }
    // The root must be an object.
    assert!(
        lower(
            &json!({"type":"array","items":{"type":"string"}}),
            &OPENAI_STRICT
        )
        .is_err()
    );
    // A closed schema with optional fields and bounds lowers.
    let schema: Value = serde_json::from_str(EXAMPLE_OUTPUT).expect("schema");
    let mut optional = schema.clone();
    optional["required"] = json!(["summary"]);
    lower(&schema, &OPENAI_STRICT).expect("lowers");
    let lowered = lower(&optional, &OPENAI_STRICT).expect("lowers");
    assert_eq!(lowered.nulls_added.len(), 1, "{:?}", lowered.nulls_added);
}

#[test]
fn the_pack_validates_against_the_agent_skills_specification() {
    for file in pack().expect("pack") {
        if let Some(dir) = file.path.strip_suffix("/SKILL.md") {
            assert_eq!(validate_skill(dir, &file.contents), Vec::<String>::new());
            // Only the standard fields.
            let frontmatter = file.contents.split("\n---\n").next().expect("frontmatter");
            let keys: Vec<&str> = frontmatter
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':').map(|(k, _)| k))
                .collect();
            assert_eq!(keys, ["name", "description"]);
            // Every reference it links to is in the pack.
            for link in file.contents.split("](").skip(1) {
                let target = link.split(')').next().expect("link");
                let path = format!("{dir}/{target}");
                assert!(
                    pack().expect("pack").iter().any(|f| f.path == path),
                    "SKILL.md links to {target}, which the pack does not have"
                );
            }
        }
    }
}

#[test]
fn the_specification_check_refuses_what_the_specification_refuses() {
    let skill = |frontmatter: &str| format!("---\n{frontmatter}\n---\nBody\n");
    let ok = skill("name: good-skill\ndescription: Does a thing.");
    assert!(validate_skill("good-skill", &ok).is_empty());
    for (dir, text) in [
        ("other-name", ok.clone()),
        ("Bad-Skill", skill("name: Bad-Skill\ndescription: x")),
        ("-bad", skill("name: -bad\ndescription: x")),
        ("bad--skill", skill("name: bad--skill\ndescription: x")),
        ("bad_skill", skill("name: bad_skill\ndescription: x")),
        ("good-skill", skill("name: good-skill")),
        ("good-skill", skill("name: good-skill\ndescription: \"\"")),
        (
            "good-skill",
            skill(&format!(
                "name: good-skill\ndescription: {}",
                "x".repeat(1025)
            )),
        ),
        (
            "good-skill",
            skill("name: good-skill\ndescription: x\nversion: 1"),
        ),
        (
            "good-skill",
            skill(&format!(
                "name: good-skill\ndescription: x\ncompatibility: {}",
                "x".repeat(501)
            )),
        ),
        ("good-skill", "name: good-skill\n".to_owned()),
        (
            "good-skill",
            "---\nname: good-skill\ndescription: x\n".to_owned(),
        ),
    ] {
        assert!(!validate_skill(dir, &text).is_empty(), "{dir}: {text}");
    }
    let long = "a".repeat(65);
    assert!(!validate_skill(&long, &skill(&format!("name: {long}\ndescription: x"))).is_empty());
}

/// The files the worked example in SKILL.md names, in order.
fn worked_example() -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut lines = CREATE_ATOM_SKILL.lines();
    while let Some(line) = lines.next() {
        let Some(path) = line
            .strip_prefix("File `")
            .and_then(|rest| rest.strip_suffix("`:"))
        else {
            continue;
        };
        let fence = lines
            .by_ref()
            .find(|l| l.starts_with("```"))
            .expect("a block follows");
        assert!(fence.len() > 3, "{path}: the block names its language");
        let body: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        files.push((path.to_owned(), body.join("\n") + "\n"));
    }
    files
}

#[test]
fn the_worked_example_checks_renders_and_its_cases_hold() {
    let files = worked_example();
    assert!(files.len() >= 6, "{files:?}");
    let dir = scratch("worked-example");
    for (path, contents) in &files {
        let target = dir.join(path);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(target, contents).expect("write");
    }
    crate::check(Some(dir.clone()), false).expect("atomic check passes");
    let (atom_file, _) = files
        .iter()
        .find(|(p, _)| p.ends_with(".atom.astro"))
        .expect("an atom");
    let (input_file, _) = files
        .iter()
        .find(|(p, _)| p.ends_with(".input.json"))
        .expect("an input");
    crate::render(crate::RenderArgs {
        atom: dir.join(atom_file),
        input: dir.join(input_file),
        run_id: None,
        step_id: "preview".into(),
        context_settings: None,
        profile: None,
    })
    .expect("atomic render works");

    // Each case's input matches the schema and its assertions check; a case
    // whose input is the typed example's holds for that example's output.
    let mut registry = Registry::new();
    let atom = atom::load(&dir.join(atom_file), &mut registry).expect("loads");
    let compiled = compile::compile(&atom, &registry).expect("compiles");
    let input_schema = registry.inline(&atom.input).expect("input");
    let output_schema = registry.inline(&atom.output).expect("output");
    let scope = atomic_schema::expr::Scope::new()
        .with("input", &input_schema)
        .with("output", &output_schema);
    let mut cases = 0;
    let mut held = 0;
    for (path, contents) in files.iter().filter(|(p, _)| p.starts_with("evals/")) {
        cases += 1;
        let case = atomic_schema::yaml::load(Path::new(path), contents)
            .expect("case YAML")
            .value;
        registry
            .validator(&atom.input)
            .expect("validator")
            .validate(&case["input"])
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        let expect = case["expect"].as_array().expect("expect list");
        assert!(!expect.is_empty(), "{path}");
        let example = compiled.examples.iter().find(|e| e.input == case["input"]);
        for assertion in expect {
            let source = assertion.as_str().expect("a string");
            atomic_schema::expr::parse(source)
                .and_then(|e| e.check(&scope))
                .unwrap_or_else(|e| panic!("{path}: {source}: {e}"));
            if let Some(example) = example {
                assert_eq!(
                    evaluate(source, &example.input, &example.output),
                    cel::Value::Bool(true),
                    "{path}: {source}"
                );
                held += 1;
            }
        }
    }
    assert!(
        cases >= 2 && held > 0,
        "{cases} cases, {held} checked assertions"
    );
}

#[test]
fn the_pack_names_only_commands_this_binary_has() {
    use clap::CommandFactory;
    let cli = crate::Cli::command();
    let commands: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
    for file in pack().expect("pack") {
        for (i, _) in file.contents.match_indices("`atomic ") {
            let word: String = file.contents[i + 8..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            assert!(
                commands.contains(&word.as_str()),
                "{} names `atomic {word}`, which this binary does not have",
                file.path
            );
        }
    }
}
