// SPDX-License-Identifier: Apache-2.0

use super::*;

fn store() -> (LocalStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "atomic-artifacts-{}-{}-{}",
        std::process::id(),
        random_name().expect("a name"),
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

#[cfg(unix)]
#[test]
fn a_stored_blob_is_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let (s, dir) = store();
    let m = s.put(&mut &b"content"[..], provenance("a")).expect("put");
    let mode = std::fs::metadata(s.blob_path(&m.artifact))
        .expect("blob")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o444);
    let _ = std::fs::remove_dir_all(dir);
}

/// A blob changed under its digest (through a link a command was given,
/// say) is replaced by the next honest put, and reading it fails until then.
#[cfg(unix)]
#[test]
fn a_damaged_blob_is_refused_on_read_and_replaced_by_an_honest_put() {
    use std::os::unix::fs::PermissionsExt;
    let (s, dir) = store();
    let m = s.put(&mut &b"content"[..], provenance("a")).expect("put");
    let blob = s.blob_path(&m.artifact);
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::fs::write(&blob, b"content and more").expect("damage");
    let mut read = Vec::new();
    let e = s
        .open(&m.artifact)
        .expect("open")
        .read_to_end(&mut read)
        .expect_err("a damaged blob fails at its end");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
    s.put(&mut &b"content"[..], provenance("b"))
        .expect("put again");
    let mut read = Vec::new();
    s.open(&m.artifact)
        .expect("open")
        .read_to_end(&mut read)
        .expect("whole again");
    assert_eq!(read, b"content");
    let _ = std::fs::remove_dir_all(dir);
}

/// A read into an empty buffer is not the end of a blob.
#[test]
fn an_empty_read_of_a_healthy_blob_succeeds() {
    let (s, dir) = store();
    let m = s.put(&mut &b"content"[..], provenance("a")).expect("put");
    let mut reader = s.open(&m.artifact).expect("open");
    assert_eq!(reader.read(&mut []).expect("an empty read"), 0);
    let mut read = Vec::new();
    reader.read_to_end(&mut read).expect("whole");
    assert_eq!(read, b"content");
    let _ = std::fs::remove_dir_all(dir);
}

/// Each store's id, from the owners it has taken.
fn owner_ids(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join("artifact-owners"))
        .expect("owners")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// A read-only file left in staging, as a process killed mid-write leaves
/// one, under the name the store would pick next, does not stop later
/// writes.
#[cfg(unix)]
#[test]
fn a_leftover_in_staging_does_not_stop_later_writes() {
    use std::os::unix::fs::PermissionsExt;
    let (s, dir) = store();
    let [id] = &owner_ids(&dir)[..] else {
        panic!("one owner")
    };
    // Every name the old scheme gave, and a few of this store's own.
    for name in (0..20)
        .map(|i| format!("{}-{i}", std::process::id()))
        .chain((0..20).map(|i| format!("{id}-{i}")))
        .chain(["0".repeat(32)].into_iter().map(|r| format!("{id}-{r}")))
    {
        let path = dir.join("staging").join(name);
        std::fs::write(&path, b"left").expect("leftover");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    }
    for i in 0..20 {
        s.put(&mut format!("content {i}").as_bytes(), provenance("make"))
            .expect("a later write");
    }
}

/// Opening a store removes what a dead store left in staging once it is
/// old enough, and nothing of a live store's.
#[test]
fn opening_a_store_sweeps_only_dead_stores_leftovers() {
    let (live, dir) = store();
    let [live_id] = &owner_ids(&dir)[..] else {
        panic!("one owner")
    };
    let live_id = live_id.clone();
    // A store that died: its owner file is there, and no one holds it.
    let dead_id = "d".repeat(32);
    std::fs::write(dir.join("artifact-owners").join(&dead_id), b"").expect("dead owner");
    let leftovers = [
        format!("{dead_id}-1"),
        format!("{}-7", std::process::id()),
        format!("{live_id}-1"),
    ];
    for name in &leftovers {
        std::fs::write(dir.join("staging").join(name), b"left").expect("leftover");
    }
    let staged = |name: &str| dir.join("staging").join(name).exists();
    // Each store's holds: the live one's, and the dead one's.
    let live_holds = live.owner.held.clone();
    let _hold = live
        .hold(&ArtifactRef::from_digest(&"a".repeat(64)).expect("digest"))
        .expect("hold");
    let dead_holds = dir.join("artifact-held").join(&dead_id);
    std::fs::create_dir_all(&dead_holds).expect("dead holds");
    std::fs::write(dead_holds.join(format!("{}-1", "b".repeat(64))), b"").expect("hold");
    // Too new to judge: kept.
    let again = LocalStore::open(&dir).expect("open again");
    assert!(leftovers.iter().all(|n| staged(n)), "a new file was swept");
    assert!(dead_holds.exists(), "a new owner's holds were cleared");
    assert!(
        dir.join("artifact-owners").join(&dead_id).exists(),
        "a new owner was judged"
    );
    drop(again);
    // Old enough: the dead store's go, with its owner, and the live one's
    // stay.
    let swept = LocalStore::open_with_grace(&dir, std::time::Duration::ZERO).expect("sweep");
    assert!(!staged(&leftovers[0]), "the dead store's file was kept");
    assert!(!staged(&leftovers[1]), "an ownerless file was kept");
    assert!(staged(&leftovers[2]), "the live store's file was swept");
    assert!(!dir.join("artifact-owners").join(&dead_id).exists());
    assert!(dir.join("artifact-owners").join(&live_id).exists());
    assert!(!dead_holds.exists(), "the dead store's holds were kept");
    assert!(live_holds.is_dir(), "the live store's holds were cleared");
    drop((swept, live));
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
    // In an atomic home (one with its private/), before the project's
    // private directory exists as after: the layout says where the lock is.
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("private")).expect("home");
    let state = home.join("projects/p");
    let private = home.join("private/p");
    let store = LocalStore::open(state.join("artifacts")).expect("open");
    assert_eq!(store.lock, private.join("artifacts.lock"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&private)
            .expect("private")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        // A private directory others may read is refused, and left as it is.
        let open = home.join("private/q");
        std::fs::create_dir_all(&open).expect("q");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(LocalStore::open(home.join("projects/q/artifacts")).is_err());
        let mode = std::fs::metadata(&open).expect("q").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "{mode:o}");
    }
    let again = LocalStore::open(state.join("artifacts")).expect("open");
    assert_eq!(again.lock, store.lock);
    // Shaped like one, but in no atomic home: its lock stays in its root, and
    // nothing is made beside it.
    let elsewhere = dir.join("not-a-home");
    let loose = LocalStore::open(elsewhere.join("projects/p/artifacts")).expect("open");
    assert_eq!(loose.lock, elsewhere.join("projects/p/artifacts/lock"));
    assert!(!elsewhere.join("private").exists());
    let loose = LocalStore::open(dir.join("artifacts")).expect("open");
    assert_eq!(loose.lock, dir.join("artifacts/lock"));
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

#[cfg(unix)]
#[test]
fn listing_what_is_stored_fails_on_an_entry_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;
    let (store, dir) = store();
    let a = store
        .put(&mut &b"behind a closed door"[..], provenance("s1"))
        .expect("put")
        .artifact;
    assert_eq!(store.stored().expect("listed"), BTreeSet::from([a.clone()]));
    let prefix = store.blob_path(&a).parent().expect("prefix").to_owned();
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let listed = store.stored();
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        listed.is_err(),
        "an unreadable prefix was left out: {listed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refresh_says_whether_it_marked_found_nothing_or_was_held_off() {
    let (store, dir) = store();
    let a = store
        .put(&mut &b"refreshed"[..], provenance("s1"))
        .expect("put")
        .artifact;
    assert_eq!(store.refresh(&a).expect("refresh"), Refreshed::Now);
    let missing = ArtifactRef::from_digest(&"0".repeat(64)).expect("ref");
    assert_eq!(
        store.refresh(&missing).expect("refresh"),
        Refreshed::Missing
    );
    let holder = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&store.lock)
        .expect("lock");
    holder.lock().expect("hold");
    assert_eq!(store.refresh(&a).expect("refresh"), Refreshed::Held);
    drop(holder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sweep leaves what an open store has staged, however old, and clears
/// what a store that is gone left.
#[test]
fn a_sweep_leaves_an_open_stores_partial_writes() {
    let (s, dir) = store();
    let in_flight = s.stage(&mut &b"in flight"[..]).expect("stage");
    std::fs::write(
        dir.join("staging").join(format!("{}-1", "e".repeat(32))),
        b"half",
    )
    .expect("a gone store's partial write");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600 * 24);
    let swept = s
        .sweep(&BTreeSet::new(), Grace::none_for_tests(), later)
        .expect("sweep");
    assert_eq!(swept.partial, 1, "{swept:?}");
    assert!(
        in_flight.path.exists(),
        "an open store's partial write was swept"
    );
    let m = s
        .commit(in_flight, &[provenance("late")])
        .expect("committed after the sweep");
    assert!(s.contains(&m[0].artifact).expect("contains"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A blob committed after a sweep began reading what to keep is kept, however
/// long before it was staged: a gather may stage for a long time before it
/// commits.
#[test]
fn a_blob_committed_after_a_sweep_began_reading_is_kept() {
    let (s, dir) = store();
    let staged = s.stage(&mut &b"staged long ago"[..]).expect("stage");
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::open(&staged.path)
        .and_then(|f| f.set_modified(long_ago))
        .expect("backdate");
    let read_at = std::time::SystemTime::now();
    let m = s.commit(staged, &[provenance("late")]).expect("commit");
    let swept = s
        .sweep(
            &BTreeSet::new(),
            Grace::at_least(std::time::Duration::from_secs(60)),
            read_at,
        )
        .expect("sweep");
    assert!(swept.removed.is_empty(), "{swept:?}");
    assert!(s.contains(&m[0].artifact).expect("contains"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A hold keeps content from every sweep, however old, until it is let go
/// or its store is gone; a dead store's holds, and ones whose owner is gone
/// once they are old, are cleared.
#[test]
fn a_hold_keeps_content_until_it_is_let_go_or_its_store_is_gone() {
    let (s, dir) = store();
    let day = std::time::Duration::from_secs(24 * 3600);
    let put = |bytes: &[u8]| {
        let a = s
            .put(&mut &bytes[..], provenance("make"))
            .expect("put")
            .artifact;
        written_ago(&s.blob_path(&a), day);
        a
    };
    let sweep = || {
        s.sweep(
            &BTreeSet::new(),
            Grace::none_for_tests(),
            std::time::SystemTime::now(),
        )
        .expect("sweep")
    };
    let (held, dead, orphaned) = (put(b"held"), put(b"dead"), put(b"orphaned"));
    let hold = s.hold(&held).expect("hold");
    // A store that died a day ago, with a hold.
    let dead_id = "d".repeat(32);
    let dead_owner = dir.join("artifact-owners").join(&dead_id);
    std::fs::write(&dead_owner, b"").expect("dead owner");
    written_ago(&dead_owner, day);
    let dead_holds = dir.join("artifact-held").join(&dead_id);
    std::fs::create_dir_all(&dead_holds).expect("dead holds");
    std::fs::write(dead_holds.join(format!("{}-1", dead.digest())), b"").expect("hold");
    // Holds whose owner file is gone: a store may be opening.
    let orphan = dir.join("artifact-held").join("e".repeat(32));
    std::fs::create_dir_all(&orphan).expect("orphan");
    std::fs::write(orphan.join(format!("{}-1", orphaned.digest())), b"").expect("hold");
    let swept = sweep();
    assert_eq!(swept.removed, vec![dead.clone()], "{swept:?}");
    assert!(!dead_holds.exists(), "a dead store's holds were kept");
    assert!(orphan.exists(), "a new store's holds were cleared");
    // Once old, holds with no owner go.
    written_ago(&orphan, day);
    assert_eq!(sweep().removed, vec![orphaned]);
    assert!(!orphan.exists());
    // Let go, the content goes.
    assert!(sweep().removed.is_empty());
    drop(hold);
    assert_eq!(sweep().removed, vec![held]);
    // A store's holds go with it.
    let other = LocalStore::open(&dir).expect("another store");
    let mine = other.hold(&dead).expect("hold");
    let own = other.owner.held.clone();
    assert!(own.is_dir());
    std::mem::forget(mine);
    drop(other);
    assert!(!own.exists(), "a closed store's holds were left");
    let _ = std::fs::remove_dir_all(dir);
}

/// A hold is taken under the store's shared lock: while a sweep holds the
/// lock, a hold waits for it, and gives up as Locked past its wait.
#[test]
fn a_hold_waits_out_a_sweep_in_progress() {
    let (s, dir) = store();
    let s = s.waiting_at_most(std::time::Duration::from_millis(100));
    let a = ArtifactRef::from_digest(&"c".repeat(64)).expect("digest");
    let sweeping = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lock"))
        .expect("the store's lock");
    sweeping.lock().expect("a sweep's lock");
    assert!(
        matches!(s.hold(&a), Err(ArtifactError::Locked { .. })),
        "held while a sweep held the store"
    );
    assert!(owner_holds(&s).is_empty());
    drop(sweeping);
    let hold = s.hold(&a).expect("held once the sweep let go");
    assert_eq!(owner_holds(&s).len(), 1);
    drop(hold);
    let _ = std::fs::remove_dir_all(dir);
}

/// The holds a store has written.
fn owner_holds(s: &LocalStore) -> Vec<PathBuf> {
    std::fs::read_dir(&s.owner.held)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}
