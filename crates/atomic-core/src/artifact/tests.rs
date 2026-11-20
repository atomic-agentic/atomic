// SPDX-License-Identifier: Apache-2.0

use super::*;

fn store() -> (LocalStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "atomic-artifacts-{}-{}-{}",
        std::process::id(),
        STAGING_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
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

#[test]
fn a_sweep_keeps_what_it_is_told_and_what_is_recent_and_clears_stale_partial_writes() {
    let root = std::env::temp_dir().join(format!("atomic-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = LocalStore::open(&root).expect("open");
    let put = |bytes: &[u8]| {
        store
            .put(&mut &bytes[..], Provenance::default())
            .expect("put")
            .artifact
    };
    let (kept, loose) = (put(b"kept"), put(b"loose"));
    // A stale partial write, a stray file, a prefix that is a link and a
    // prefix that is not two hex digits: none of them is content.
    std::fs::write(root.join("staging/1234-0"), b"half").expect("partial");
    let blobs = root.join("blobs/sha256");
    std::fs::write(blobs.join(&kept.digest()[..2]).join("not-a-digest"), b"x").expect("stray");
    let elsewhere = root.with_extension("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("elsewhere");
    let hidden = format!("{:0>62}", "1");
    std::fs::write(elsewhere.join(&hidden), b"outside the store").expect("outside");
    std::os::unix::fs::symlink(&elsewhere, blobs.join("ff")).expect("link");
    std::fs::create_dir_all(blobs.join("zz")).expect("odd prefix");

    // A grace of an hour keeps everything just written.
    let swept = store
        .sweep(
            &BTreeSet::new(),
            Grace::at_least(std::time::Duration::from_secs(3600)),
            std::time::SystemTime::now(),
        )
        .expect("sweep");
    assert!(swept.removed.is_empty());
    assert_eq!(swept.partial, 0);

    let swept = store
        .sweep(
            &BTreeSet::from_iter([&kept].into_iter().cloned()),
            Grace::none_for_tests(),
            std::time::SystemTime::now(),
        )
        .expect("sweep");
    assert_eq!(swept.removed, std::slice::from_ref(&loose));
    assert_eq!(swept.partial, 1, "the stale partial write is cleared");
    assert!(!root.join("staging/1234-0").exists());
    assert!(store.contains(&kept).expect("contains"));
    assert!(!store.contains(&loose).expect("contains"));
    assert!(store.manifests(&loose).expect("manifests").is_empty());
    for skipped in ["not-a-digest", "ff", "zz"] {
        assert!(
            swept.skipped.iter().any(|p| p.ends_with(skipped)),
            "{skipped} not reported: {:?}",
            swept.skipped
        );
    }
    assert!(
        elsewhere.join(&hidden).exists(),
        "the sweep followed a link"
    );
    // Below the minimum, a grace is the minimum.
    assert_eq!(
        Grace::at_least(std::time::Duration::from_secs(1)),
        Grace::at_least(MIN_GRACE)
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

#[cfg(unix)]
#[test]
fn a_stored_blob_is_read_only_and_a_damaged_one_is_replaced_by_the_next_write() {
    use std::os::unix::fs::PermissionsExt;
    let (store, dir) = store();
    let a = store
        .put(&mut &b"the content"[..], provenance("s1"))
        .expect("put")
        .artifact;
    let blob = store.blob_path(&a);
    let mode = std::fs::metadata(&blob).expect("blob").permissions().mode() & 0o777;
    assert_eq!(mode, 0o444, "{mode:o}");
    // A command handed a link to it cannot append to it.
    let link = dir.join("link");
    std::os::unix::fs::symlink(&blob, &link).expect("link");
    assert!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&link)
            .is_err(),
        "a stored blob took a write"
    );
    // Its owner can still damage it, on purpose: the size, then the bytes.
    for damage in [&b" and more"[..], &b"THE"[..]] {
        std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let mut f = if damage.len() > 3 {
            std::fs::OpenOptions::new().append(true).open(&blob)
        } else {
            std::fs::OpenOptions::new().write(true).open(&blob)
        }
        .expect("open");
        f.write_all(damage).expect("damage");
        drop(f);
        assert_ne!(std::fs::read(&blob).expect("read"), b"the content");
        // Written again, the content replaces what was damaged.
        store
            .put(&mut &b"the content"[..], provenance("s2"))
            .expect("put again");
        assert_eq!(std::fs::read(&blob).expect("read"), b"the content");
        let mode = std::fs::metadata(&blob).expect("blob").permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "{mode:o}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Date `path` `ago` back.
fn written_ago(path: &Path, ago: std::time::Duration) {
    std::fs::File::open(path)
        .expect("open")
        .set_modified(std::time::SystemTime::now() - ago)
        .expect("date");
}

#[test]
fn the_grace_runs_back_from_when_the_keep_set_was_read() {
    let (store, dir) = store();
    let a = store
        .put(
            &mut &b"stored while the keep set was read"[..],
            provenance("s1"),
        )
        .expect("put")
        .artifact;
    let minute = std::time::Duration::from_secs(60);
    // Written ten minutes ago; the caller began reading what to keep nine
    // and a half minutes ago, and took that long to reach the sweep.
    written_ago(&store.blob_path(&a), 10 * minute);
    let read_at = std::time::SystemTime::now() - (10 * minute - minute / 2);
    let swept = store
        .sweep(&BTreeSet::new(), Grace::at_least(minute), read_at)
        .expect("sweep");
    assert!(swept.removed.is_empty(), "{swept:?}");
    assert!(store.contains(&a).expect("contains"));
    // Read now, it is past its grace.
    let swept = store
        .sweep(
            &BTreeSet::new(),
            Grace::at_least(minute),
            std::time::SystemTime::now(),
        )
        .expect("sweep");
    assert_eq!(swept.removed, [a]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_projects_store_takes_its_lock_in_private_however_it_is_opened() {
    let (_, dir) = store();
    let state = dir.join("state");
    std::fs::create_dir_all(state.join("private")).expect("private");
    let store = LocalStore::open(state.join("artifacts")).expect("open");
    assert_eq!(store.lock, state.join("private/artifacts.lock"));
    let other = LocalStore::open(dir.join("elsewhere")).expect("open");
    assert_eq!(other.lock, dir.join("elsewhere/lock"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_writable_blob_is_made_read_only_when_written_again_keeping_its_readers() {
    use std::os::unix::fs::PermissionsExt;
    let (store, dir) = store();
    let a = store
        .put(&mut &b"left writable"[..], provenance("s1"))
        .expect("put")
        .artifact;
    let blob = store.blob_path(&a);
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o640)).expect("chmod");
    store
        .put(&mut &b"left writable"[..], provenance("s2"))
        .expect("put again");
    let mode = std::fs::metadata(&blob).expect("blob").permissions().mode() & 0o777;
    assert_eq!(mode, 0o440, "{mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_write_that_cannot_check_the_stored_blob_leaves_no_partial_write() {
    use std::os::unix::fs::PermissionsExt;
    let (store, dir) = store();
    let a = store
        .put(&mut &b"unreadable later"[..], provenance("s1"))
        .expect("put")
        .artifact;
    let blob = store.blob_path(&a);
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let refused = store.put(&mut &b"unreadable later"[..], provenance("s2"));
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        files_under(&dir.join("staging")),
        0,
        "a partial write was left"
    );
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    let _ = std::fs::remove_dir_all(&dir);
}
