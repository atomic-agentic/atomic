// SPDX-License-Identifier: Apache-2.0

//! Interpreted scorer bindings with full schema gates around every boundary.
//! Calling the judge model belongs to a sibling workflow step, outside this module.

use crate::atom::Atom;
use atomic_schema::registry::Registry;
use serde_json::{Value, json};

/// Candidate and frozen host data used to construct a judge input.
pub struct Evaluation<'a> {
    /// Expected run identity, supplied independently by the evaluation caller.
    pub run_id: &'a str,
    /// Full input value.
    pub input: &'a Value,
    /// Full candidate output.
    pub output: &'a Value,
    /// Evaluation case metadata.
    pub case: &'a Value,
    /// Optional frozen public context.
    pub snapshot: Option<&'a crate::context::ContextSnapshot>,
    /// Trusted current step identity.
    pub step_id: Option<&'a str>,
}

fn options(
    atom: &Atom,
    snapshot: Option<&crate::context::ContextSnapshot>,
    step_id: Option<&str>,
    run_id: &str,
) -> Result<Value, String> {
    if let Some(snapshot) = snapshot {
        snapshot.verify_for(&atom.program.hash(), run_id)?;
    }
    Ok(json!({"contextSnapshot":snapshot,"stepId":step_id}))
}

fn judge<'a>(atom: &'a Atom, scorer: &str) -> Result<&'a Value, String> {
    let name = atom
        .frontmatter
        .get("scorers")
        .and_then(|value| value.get(scorer))
        .and_then(|value| value.get("judge"))
        .and_then(Value::as_str)
        .ok_or("unknown scorer")?;
    atom.judges
        .get(name)
        .ok_or_else(|| "undeclared judge".into())
}

/// Build a judge input only after validating the candidate, case and resulting binding.
pub fn judge_input(
    atom: &Atom,
    _registry: &Registry,
    scorer: &str,
    values: &Evaluation<'_>,
) -> Result<Value, String> {
    let options = options(atom, values.snapshot, values.step_id, values.run_id)?;
    for (key, value) in [("Input", values.input), ("Output", values.output)] {
        jsonschema::validator_for(&atom.program.contracts[key])
            .map_err(|e| e.to_string())?
            .validate(value)
            .map_err(|e| e.to_string())?;
    }
    jsonschema::validator_for(&atom.case)
        .map_err(|e| e.to_string())?
        .validate(values.case)
        .map_err(|e| e.to_string())?;
    let result = atom.program.invoke("judgeInput", json!({"scorer":scorer,"values":{"input":values.input,"output":values.output,"case":values.case},"options":options}))?;
    jsonschema::validator_for(&judge(atom, scorer)?["input"])
        .map_err(|e| e.to_string())?
        .validate(&result)
        .map_err(|e| e.to_string())?;
    Ok(result)
}

/// Validate the complete judge output before interpreting its finite normalized score.
pub fn judge_score(
    atom: &Atom,
    scorer: &str,
    output: &Value,
    snapshot: Option<&crate::context::ContextSnapshot>,
    step_id: Option<&str>,
    run_id: &str,
) -> Result<f64, String> {
    let options = options(atom, snapshot, step_id, run_id)?;
    jsonschema::validator_for(&judge(atom, scorer)?["output"])
        .map_err(|e| e.to_string())?
        .validate(output)
        .map_err(|e| e.to_string())?;
    atom.program
        .invoke(
            "judgeScore",
            json!({"scorer":scorer,"values":output,"options":options}),
        )?
        .as_f64()
        .ok_or_else(|| "judge score is not numeric".into())
}

/// Compute the bounded mean and enforce the declared threshold.
pub fn aggregate(atom: &Atom, scorer: &str, scores: &[f64]) -> Result<Value, String> {
    if scores.iter().any(|score| !score.is_finite()) {
        return Err("non-finite score".into());
    }
    atom.program
        .invoke("aggregate", json!({"scorer":scorer,"scores":scores}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_boundaries_enforce_full_schemas_and_thresholds() {
        let root = crate::atom::tests::project("{input.text}", "{}", None);
        std::fs::write(root.join("test.atom.astro"), r#"---
import type { Props, Input, Output, Case, Judges } from './.atomic/types/test';
import type { AtomConfig } from './.atomic/types/authoring';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output, Case, Judges> = {
  case: {type:'object',additionalProperties:false,required:['ok'],properties:{ok:{type:'boolean'}}},
  judges: {reviewer: {
    input: {type:'object',additionalProperties:false,required:['text'],properties:{text:{type:'string',maxLength:3}}},
    output: {type:'object',additionalProperties:false,required:['ok'],properties:{ok:{type:'boolean'}}},
  }},
  scorers: {quality: {judge:'reviewer',with:({input}) => ({text:input.text}),score:({judge}) => judge.ok ? 1 : 0,min_mean:0.75}},
};
---
{input.text}
"#).expect("source");
        let mut registry = Registry::new();
        let atom = crate::atom::load(&root.join("test.atom.astro"), &mut registry).expect("types");
        let input = json!({"text":"abc"});
        let output = json!({"ticket":"ABC-1"});
        let case = json!({"ok":true});
        let values = Evaluation {
            run_id: "evaluation-test",
            input: &input,
            output: &output,
            case: &case,
            snapshot: None,
            step_id: None,
        };
        assert_eq!(
            judge_input(&atom, &registry, "quality", &values).expect("binding"),
            json!({"text":"abc"})
        );
        let long = json!({"text":"too long"});
        let values = Evaluation {
            run_id: "evaluation-test",
            input: &long,
            ..values
        };
        assert!(
            judge_input(&atom, &registry, "quality", &values).is_err(),
            "full judge input maxLength"
        );
        assert_eq!(
            judge_score(
                &atom,
                "quality",
                &json!({"ok":true}),
                None,
                None,
                "evaluation-test"
            )
            .expect("score"),
            1.0
        );
        assert!(
            judge_score(
                &atom,
                "quality",
                &json!({"ok":true,"extra":true}),
                None,
                None,
                "evaluation-test"
            )
            .is_err()
        );
        assert_eq!(
            aggregate(&atom, "quality", &[1.0, 0.0]).expect("mean")["passed"],
            false
        );
        assert!(aggregate(&atom, "quality", &[]).is_err());
        assert!(aggregate(&atom, "quality", &[f64::NAN]).is_err());
    }
}
