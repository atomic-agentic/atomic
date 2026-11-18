// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::atom;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const SUMMARIZE: &str = r#"---
import type { Props } from "./.atomic/types/summarize-note";
const { input }: Props = Astro.props;
export const config = {"atomic": 1, "description": "Summarize one note.", "kind": "lite", "input": {"type": "object", "additionalProperties": false, "required": ["text"], "properties": {"text": {"type": "string"}}}, "output": {"type": "object", "additionalProperties": false, "required": ["summary"], "properties": {"summary": {"type": "string"}}}};
---
Summarize: {input.text}
"#;

const REVIEW: &str = r#"---
import type { Props } from "./.atomic/types/review-summary";
const { input }: Props = Astro.props;
export const config = {"atomic": 1, "description": "Review a summary against its source.", "kind": "lite", "input": {"type": "object", "additionalProperties": false, "required": ["source", "draft"], "properties": {"source": {"type": "string"}, "draft": {"type": "string"}}}, "output": {"type": "object", "additionalProperties": false, "required": ["verdict"], "properties": {"verdict": {"enum": ["ok", "revise"]}, "notes": {"type": "string"}}}};
---
Source: {input.source}
Draft: {input.draft}
"#;

const WORKFLOW: &str = "atomic: 1
input:
  type: object
  required: [text]
  properties:
    text: {type: string}
output:
  type: object
  required: [verdict]
  properties:
    verdict: {enum: [ok, revise]}
    notes: {type: string}
steps:
  summarize:
    atom: summarize-note
    with: {text: input.text}
  review:
    after: [summarize]
    atom: review-summary
    with:
      source: input.text
      draft: steps.summarize.output.summary
  accept:
    after: [review]
    gate:
      title: \"'Accept the reviewed summary'\"
      show: \"{'draft': steps.summarize.output, 'review': steps.review.output}\"
      timeout: 24h
      on_timeout: reject
returns: steps.review.output
";

fn check(workflow: &str) -> Result<Workflow, WorkflowError> {
    let root = std::env::temp_dir().join(format!(
        "atomic-workflow-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    for (p, t) in [
        ("atoms/summarize-note.atom.astro", SUMMARIZE),
        ("atoms/review-summary.atom.astro", REVIEW),
        ("workflows/summarize-and-review.workflow.yaml", workflow),
    ] {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, t).expect("write");
    }
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for name in ["summarize-note", "review-summary"] {
        let a = atom::load(
            &root.join(format!("atoms/{name}.atom.astro")),
            &mut registry,
        )
        .expect("atom loads");
        atoms.insert(a.name.clone(), a);
    }
    load(
        &root.join("workflows/summarize-and-review.workflow.yaml"),
        &atoms,
        &mut registry,
    )
}

fn err(workflow: &str) -> WorkflowError {
    check(workflow).expect_err("should fail")
}

#[test]
fn a_well_formed_workflow_loads_in_dependency_order() {
    let w = check(WORKFLOW).expect("loads");
    assert_eq!(w.name, "summarize-and-review");
    assert_eq!(w.order, ["summarize", "review", "accept"]);
    assert!(w.warnings.is_empty(), "{:?}", w.warnings);
}

#[test]
fn a_misspelled_atom_reference_fails() {
    let e = err(&WORKFLOW.replace("atom: summarize-note", "atom: summarise-note"));
    assert_eq!((e.path.as_str(), e.line), ("/steps/summarize/atom", 15));
    assert!(
        e.message
            .contains("no atom named `summarise-note`; did you mean summarize-note?"),
        "{}",
        e.message
    );
}

#[test]
fn a_mapping_that_cannot_fit_fails() {
    let e = err(&WORKFLOW.replace(
        "draft: steps.summarize.output.summary",
        "draft: steps.summarize.output",
    ));
    assert_eq!(e.path, "/steps/review/with/draft");
    assert!(
        e.message
            .contains("cannot fit `draft`: it is object and the field takes string"),
        "{}",
        e.message
    );
}

#[test]
fn bindings_are_checked_against_the_atoms_input() {
    let e = err(&WORKFLOW.replace("with: {text: input.text}", "with: {txt: input.text}"));
    assert!(
        e.message
            .contains("has no input field `txt` (fields: text)"),
        "{}",
        e.message
    );
    let e = err(&WORKFLOW.replace("      draft: steps.summarize.output.summary\n", ""));
    assert!(
        e.message.contains("requires input field `draft`"),
        "{}",
        e.message
    );
    let e = err(&WORKFLOW.replace("with: {text: input.text}", "with: {text: input.txt}"));
    assert!(
        e.message.contains("`input` has no field `txt`"),
        "{}",
        e.message
    );
}

#[test]
fn a_step_sees_only_the_steps_it_runs_after() {
    let e = err(&WORKFLOW.replace(
        "with: {text: input.text}",
        "with: {text: steps.review.output.notes}",
    ));
    assert_eq!(e.path, "/steps/summarize/with/text");
    assert!(
        e.message.contains("`steps` has no field `review`"),
        "{}",
        e.message
    );
}

#[test]
fn cycles_unknown_dependencies_and_bad_results_fail() {
    let e = err(&WORKFLOW.replace(
        "    atom: summarize-note\n",
        "    after: [accept]\n    atom: summarize-note\n",
    ));
    assert!(e.message.contains("cycle"), "{}", e.message);
    let e = err(&WORKFLOW.replace("after: [summarize]", "after: [summarise]"));
    assert!(e.message.contains("not a step here"), "{}", e.message);
    let e = err(&WORKFLOW.replace(
        "returns: steps.review.output",
        "returns: steps.summarize.output.summary",
    ));
    assert_eq!(e.path, "/returns");
    assert!(
        e.message.contains("cannot fit the workflow's output"),
        "{}",
        e.message
    );
    let e = err(&WORKFLOW.replace("atomic: 1\n", ""));
    assert!(e.message.contains("format version"), "{}", e.message);
}

#[test]
fn schema_inlining_preserves_reference_sibling_constraints() {
    let schema =
        serde_json::json!({"$defs":{"text":{"type":"string"}},"$ref":"#/$defs/text","maxLength":3});
    let result = inline(&schema, Path::new("/tmp"), &mut Registry::new(), 0).expect("inline");
    assert!(
        !jsonschema::validator_for(&result)
            .expect("validator")
            .is_valid(&serde_json::json!("too long"))
    );
}

#[test]
fn schema_inlining_rejects_recursion_instead_of_weakening_the_contract() {
    let schema = serde_json::json!({"type":"object","properties":{"child":{"$ref":"#"}}});
    assert!(inline(&schema, Path::new("/tmp"), &mut Registry::new(), 0).is_err());
}

#[test]
fn schema_inlining_leaves_literal_reference_shaped_data_unchanged() {
    let schema = serde_json::json!({"const":{"$ref":"not-a-schema.json"}});
    assert_eq!(
        inline(&schema, Path::new("/tmp"), &mut Registry::new(), 0).expect("literal data"),
        schema
    );
}

#[test]
fn reference_sibling_unevaluated_properties_see_target_annotations() {
    let schema = serde_json::json!({"$defs":{"object":{"type":"object","properties":{"name":{"type":"string"}}}},"$ref":"#/$defs/object","unevaluatedProperties":false});
    let expanded = inline(&schema, Path::new("/tmp"), &mut Registry::new(), 0).expect("inline");
    let validator = jsonschema::validator_for(&expanded).expect("validator");
    assert!(validator.is_valid(&serde_json::json!({"name":"value"})));
    assert!(!validator.is_valid(&serde_json::json!({"name":"value","extra":true})));
}

#[test]
fn a_budget_is_read_and_a_bad_one_fails_where_it_is_written() {
    let w = check(&format!(
        "{WORKFLOW}budget:\n  tokens: 60000\n  minutes: 15\n"
    ))
    .expect("loads");
    assert_eq!(w.budget.tokens, Some(60_000));
    assert_eq!(w.budget.millis, Some(900_000));
    assert_eq!(w.budget.micro_usd, None);
    assert_eq!(check(WORKFLOW).expect("loads").budget, Budget::default());

    let e = err(&format!("{WORKFLOW}budget:\n  tokens: 0\n"));
    assert_eq!((e.path.as_str(), e.line), ("/budget/tokens", 32));
    assert!(e.message.contains("positive integer"), "{}", e.message);
}
