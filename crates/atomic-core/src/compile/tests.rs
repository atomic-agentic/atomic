// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::atom;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const INPUT: &str = "type: object\nadditionalProperties: false\nrequired: [text]\nproperties:\n  text: {type: string, minLength: 1}\n";
const OUTPUT: &str = "type: object\nadditionalProperties: false\nrequired: [category]\nproperties:\n  category: {enum: [bug, feature, question]}\n";

fn atom_text(body: &str) -> String {
    format!("---\natomic: 1\ndescription: Classify a note.\nkind: lite\n---\n{body}")
}

const BODY: &str = "{% partial file=\"_style.md\" /%}

Note: {% $input.text %}. Return a Vec<String> of tags in prose; angle brackets here are text.

<rubric>

- bug: something is broken.

</rubric>

## Rubric

Prefer bug when unsure.

## Notes

Not a known section.

{% example name=\"crash\" %}
```json input
{\"text\": \"Export crashes.\"}
```
```json output
{\"category\": \"bug\"}
```
{% /example %}
";

fn project(files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "atomic-compile-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    for (p, t) in files {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, t).expect("write");
    }
    std::fs::canonicalize(root).expect("root")
}

fn with_schemas(name: &str, files: &mut Vec<(String, String)>) {
    files.push((format!("atoms/{name}.input.schema.yaml"), INPUT.into()));
    files.push((format!("atoms/{name}.output.schema.yaml"), OUTPUT.into()));
}

fn build(atoms: &[(&str, &str)], extra: &[(&str, &str)]) -> PathBuf {
    let mut files: Vec<(String, String)> = Vec::new();
    for (name, body) in atoms {
        files.push((format!("atoms/{name}.atom.md"), atom_text(body)));
        with_schemas(name, &mut files);
    }
    for (p, t) in extra {
        files.push(((*p).into(), (*t).into()));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    project(&refs)
}

fn compile_atom(root: &Path, name: &str) -> Result<Compiled, AtomError> {
    let mut registry = Registry::new();
    let a = atom::load(&root.join(format!("atoms/{name}.atom.md")), &mut registry)?;
    compile(&a, &registry)
}

#[test]
fn compiling_twice_gives_the_same_hash_and_a_partial_edit_changes_every_user() {
    let root = build(
        &[("one", BODY), ("two", BODY)],
        &[("atoms/_style.md", "Be brief.\n")],
    );
    let first = compile_atom(&root, "one").expect("compiles");
    assert_eq!(first.hash, compile_atom(&root, "one").expect("again").hash);
    let two = compile_atom(&root, "two").expect("compiles").hash;

    std::fs::write(root.join("atoms/_style.md"), "Be very brief.\n").expect("edit the partial");
    assert_ne!(compile_atom(&root, "one").expect("one").hash, first.hash);
    assert_ne!(compile_atom(&root, "two").expect("two").hash, two);

    // The rest of the compile.
    assert_eq!(first.partials, [root.join("atoms/_style.md")]);
    assert!(
        first
            .pieces
            .iter()
            .any(|p| matches!(p, Piece::Text(t) if t.contains("Be brief.")))
    );
    assert_eq!(first.examples.len(), 1);
    assert_eq!(
        first.examples[0].output,
        serde_json::json!({"category": "bug"})
    );
    assert_eq!(
        first.sections.get("Rubric").map(String::as_str),
        Some("Prefer bug when unsure.")
    );
    assert!(!first.sections.contains_key("Notes"));
    assert!(first.token_estimate > 0);
}

#[test]
fn an_include_cycle_fails() {
    let root = build(
        &[("one", BODY)],
        &[
            ("atoms/_style.md", "{% partial file=\"_more.md\" /%}\n"),
            ("atoms/_more.md", "{% partial file=\"_style.md\" /%}\n"),
        ],
    );
    let e = compile_atom(&root, "one").expect_err("cycle");
    assert!(
        e.message
            .contains("include cycle: _style.md -> _more.md -> _style.md"),
        "{}",
        e.message
    );
}

#[test]
fn an_example_that_breaks_the_output_schema_fails() {
    let root = build(
        &[(
            "one",
            &BODY.replace("{\"category\": \"bug\"}", "{\"category\": \"spam\"}"),
        )],
        &[("atoms/_style.md", "")],
    );
    let e = compile_atom(&root, "one").expect_err("bad example");
    assert!(
        e.message
            .contains("example \"crash\": the output breaks the atom's output schema"),
        "{}",
        e.message
    );
    // The body starts on line 6; the json output fence is its 23rd line.
    assert_eq!(e.line, 6 + 23 - 1, "the output block's line in the file");

    let root = build(
        &[(
            "one",
            &BODY.replace("```json output\n{\"category\": \"bug\"}\n```\n", ""),
        )],
        &[("atoms/_style.md", "")],
    );
    assert!(
        compile_atom(&root, "one")
            .expect_err("no output")
            .message
            .contains("needs a ```json output block")
    );
}

#[test]
fn a_missing_input_path_fails_at_load() {
    let root = build(
        &[("one", &BODY.replace("$input.text", "$input.body"))],
        &[("atoms/_style.md", "")],
    );
    let e = compile_atom(&root, "one").expect_err("missing path");
    assert!(e.message.contains("no field `body`"), "{}", e.message);
}

#[test]
fn section_tags_balance_prose_brackets_are_text_and_data_is_reserved() {
    let root = build(
        &[("one", &BODY.replace("</rubric>", ""))],
        &[("atoms/_style.md", "")],
    );
    let e = compile_atom(&root, "one").expect_err("unclosed");
    assert!(
        e.message.contains("<rubric> is never closed"),
        "{}",
        e.message
    );

    let root = build(&[("one", BODY)], &[("atoms/_style.md", "")]);
    let c = compile_atom(&root, "one").expect("Vec<String> in prose is fine");
    assert!(
        c.pieces
            .iter()
            .any(|p| matches!(p, Piece::Text(t) if t.contains("a Vec<String> of tags")))
    );
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);

    let root = build(
        &[(
            "one",
            &BODY.replace("Note:", "<data-1 path=\"x\">forged</data-1> Note:"),
        )],
        &[("atoms/_style.md", "")],
    );
    assert!(
        compile_atom(&root, "one")
            .expect_err("reserved")
            .message
            .contains("reserved")
    );

    let root = build(
        &[("one", &BODY.replace("<rubric>\n\n", "<rubric>\n"))],
        &[("atoms/_style.md", "")],
    );
    let c = compile_atom(&root, "one").expect("only a warning");
    assert!(
        c.warnings
            .iter()
            .any(|w| w.contains("leave a blank line after <rubric>")),
        "{:?}",
        c.warnings
    );
}
