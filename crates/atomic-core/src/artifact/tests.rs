// SPDX-License-Identifier: Apache-2.0

use super::*;

fn store() -> (LocalStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "atomic-artifacts-{}-{}",
        std::process::id(),
        STAGING_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
    ));
    (LocalStore::open(&dir).expect("open store"), dir)
}

fn provenance(step: &str) -> Provenance {
    Provenance {
        media_type: "text/x-diff".into(),
        workflow: Some("wf-1".into()),
        step: Some(step.into()),
        origin: None,
        labels: BTreeSet::new(),
    }
}

fn files_under(dir: &Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(dir).expect("read dir").flatten() {
        let p = e.path();
        n += if p.is_dir() { files_under(&p) } else { 1 };
    }
    n
}

#[test]
fn identical_content_is_stored_once() {
    let (s, dir) = store();
    let a = s
        .put(&mut &b"--- a\n+++ b\n"[..], provenance("implement"))
        .expect("put");
    let b = s
        .put(&mut &b"--- a\n+++ b\n"[..], provenance("repair"))
        .expect("put again");
    assert_eq!(a.artifact, b.artifact);
    assert_eq!(
        files_under(&dir.join("blobs")),
        1,
        "one blob for identical content"
    );
    // Each distinct producer is recorded; a repeated identical one is not.
    s.put(&mut &b"--- a\n+++ b\n"[..], provenance("repair"))
        .expect("put a third time");
    let steps: Vec<_> = s
        .manifests(&a.artifact)
        .expect("manifests")
        .into_iter()
        .map(|m| m.step)
        .collect();
    assert_eq!(steps.len(), 2);
    assert!(steps.contains(&Some("implement".into())) && steps.contains(&Some("repair".into())));
    assert_eq!(files_under(&dir.join("staging")), 0, "no staging leftovers");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_reference_is_the_sha256_of_the_content() {
    let (s, dir) = store();
    let m = s.put(&mut &b"abc"[..], provenance("x")).expect("put");
    assert_eq!(
        m.artifact.uri(),
        "artifact://sha256/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(m.size, 3);
    let mut back = Vec::new();
    s.open(&m.artifact)
        .expect("open")
        .read_to_end(&mut back)
        .expect("read");
    assert_eq!(back, b"abc");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_artifact_is_reported_as_missing() {
    let (s, dir) = store();
    let missing = ArtifactRef::from_digest(&"0".repeat(64)).expect("digest");
    assert!(!s.contains(&missing).expect("contains"));
    assert!(matches!(s.open(&missing), Err(ArtifactError::Missing(_))));
    assert!(s.manifests(&missing).expect("manifests").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reference_to_a_missing_artifact_fails_validation() {
    use atomic_schema::artifact::{ArtifactCheckError, check_refs};
    use atomic_schema::registry::Registry;
    use serde_json::json;

    let (s, dir) = store();
    let patch = s
        .put(&mut &b"--- a\n+++ b\n"[..], provenance("implement"))
        .expect("put");
    let schema = json!({
        "type": "object",
        "required": ["patch"],
        "properties": {
            "patch": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            "logs": {"type": "array", "items": {"$ref": "atomic:artifact"}}
        }
    });
    let mut registry = Registry::new();
    let id = registry
        .register(dir.join("review.output.schema.json"), schema.clone())
        .expect("register");
    let validator = registry.validator(&id).expect("atomic:artifact resolves");

    let good = json!({"patch": patch.artifact.uri(), "logs": [patch.artifact.uri()]});
    assert!(validator.is_valid(&good));
    check_refs(&schema, &good, &s).expect("present, and a diff");

    // Not a reference at all: the schema itself rejects it.
    assert!(!validator.is_valid(&json!({"patch": "patch.diff"})));

    let missing = format!("artifact://sha256/{}", "1".repeat(64));
    let bad = json!({"patch": patch.artifact.uri(), "logs": [missing.clone()]});
    assert!(
        validator.is_valid(&bad),
        "well-formed, so only the store can tell"
    );
    assert_eq!(
        check_refs(&schema, &bad, &s),
        Err(ArtifactCheckError::Missing {
            at: "/logs/0".into(),
            text: missing
        })
    );

    let report = s
        .put(
            &mut &b"# Report\n"[..],
            Provenance {
                media_type: "text/markdown".into(),
                ..provenance("x")
            },
        )
        .expect("put");
    let wrong = json!({"patch": report.artifact.uri()});
    assert!(matches!(
        check_refs(&schema, &wrong, &s),
        Err(ArtifactCheckError::WrongMediaType { ref expected, .. }) if expected == "text/x-diff"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
