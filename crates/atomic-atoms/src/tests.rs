// SPDX-License-Identifier: Apache-2.0

use super::*;
use atomic_schema::lower::{self, OPENAI_STRICT};

#[test]
fn every_output_contract_lowers_to_the_strict_profile() {
    for (name, schema) in [
        (DraftTask::NAME, DraftTask.output_schema()),
        (ReviewPatch::NAME, ReviewPatch.output_schema()),
    ] {
        let lowered =
            lower::lower(&schema, &OPENAI_STRICT).unwrap_or_else(|e| panic!("{name}: {e}"));
        let problems = lower::check(&lowered.schema, &OPENAI_STRICT);
        assert!(problems.is_empty(), "{name}: {problems:?}");
    }
}

#[test]
fn each_atom_has_its_kind_and_sandbox() {
    assert_eq!(DraftTask.kind(), AtomKind::Lite);
    assert_eq!(ReviewPatch.kind(), AtomKind::Codex);
    let mut config = AtomConfig {
        sandbox: Sandbox::WorkspaceWrite,
        ..AtomConfig::default()
    };
    ReviewPatch.configure(&mut config);
    assert_eq!(
        config.sandbox,
        Sandbox::ReadOnly,
        "a reviewer never writes, whatever the project default"
    );
}

fn task(criteria: &[&str]) -> DraftedTask {
    DraftedTask {
        title: "Charge for every unit".into(),
        priority: 2,
        criteria: criteria.iter().map(|c| (*c).to_owned()).collect(),
    }
}

#[test]
fn a_drafted_task_needs_distinct_criteria_and_a_title() {
    assert_eq!(DraftTask.validate(&task(&["Two units total 250."])), Ok(()));
    assert!(matches!(
        DraftTask.validate(&task(&["Two units total 250.", " two units total 250. "])),
        Err(AtomError::Validation(m)) if m.contains("twice")
    ));
    assert!(matches!(
        DraftTask.validate(&task(&["  "])),
        Err(AtomError::Validation(_))
    ));
    let mut blank = task(&["x"]);
    blank.title = " ".into();
    assert!(DraftTask.validate(&blank).is_err());
}

fn verdict(decision: Decision, severities: &[Severity]) -> Verdict {
    Verdict {
        verdict: decision,
        summary: "Checked both criteria.".into(),
        findings: severities
            .iter()
            .map(|s| Finding {
                file: "pricing.py".into(),
                line: Some(2),
                severity: *s,
                message: "m".into(),
            })
            .collect(),
    }
}

#[test]
fn an_approval_carries_no_blocker_or_major_finding() {
    use Severity::*;
    assert_eq!(
        ReviewPatch.validate(&verdict(Decision::Approve, &[Minor, Nit])),
        Ok(())
    );
    for s in [Blocker, Major] {
        assert!(
            ReviewPatch
                .validate(&verdict(Decision::Approve, &[Nit, s]))
                .is_err()
        );
        assert_eq!(
            ReviewPatch.validate(&verdict(Decision::RequestChanges, &[s])),
            Ok(())
        );
    }
}

#[test]
fn verdicts_use_the_contracts_spelling() {
    let v = serde_json::to_value(verdict(Decision::RequestChanges, &[Severity::Blocker]))
        .expect("serializes");
    assert_eq!(v["verdict"], "request_changes");
    assert_eq!(v["findings"][0]["severity"], "blocker");
    let validator = jsonschema::validator_for(&ReviewPatch.output_schema()).expect("schema");
    assert!(validator.is_valid(&v), "{v}");
}
