// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::artifact::LocalStore;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

/// A store and a command's scratch directory, removed when the test ends.
struct Fixture {
    dir: PathBuf,
    store: LocalStore,
    exchange: Exchange,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(
            &self.exchange.inputs,
            std::fs::Permissions::from_mode(0o700),
        );
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "atomic-files-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("scratch")).expect("scratch");
    let store = LocalStore::open(dir.join("store")).expect("store");
    let exchange = Exchange::create(&dir.join("scratch")).expect("exchange");
    Fixture {
        dir,
        store,
        exchange,
    }
}

fn provenance() -> Provenance {
    Provenance {
        media_type: DEFAULT_MEDIA_TYPE.into(),
        workflow: Some("run-1".into()),
        step: Some("make".into()),
        origin: Some("make-patch".into()),
        labels: Default::default(),
    }
}

/// A contract with an artifact where `patch` is, a string where `note` is,
/// and an object of its own where `meta` is.
fn contract() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["patch"],
        "properties": {
            "patch": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            "more": {"type": "array", "items": {"$ref": "atomic:artifact"}},
            "note": {"type": "string"},
            "meta": {"type": "object"}
        }
    })
}

fn validate(value: &Value) -> Result<(), String> {
    let validator = atomic_schema::registry::Registry::new()
        .validator_for(&contract())
        .expect("validator");
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

impl Fixture {
    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.exchange.outputs.join(name), bytes).expect("write");
    }

    fn gather(&self, output: &mut Value) -> Result<Vec<Hold>, ArtifactError> {
        self.gather_within(output, Limits::default())
    }

    fn gather_within(
        &self,
        output: &mut Value,
        limits: Limits,
    ) -> Result<Vec<Hold>, ArtifactError> {
        self.exchange.gather(
            &self.store,
            &self.store,
            output,
            &contract(),
            &validate,
            &provenance(),
            limits,
        )
    }

    fn blobs(&self) -> usize {
        count_files(&self.dir.join("store/blobs"))
    }

    fn staged(&self) -> usize {
        count_files(&self.dir.join("store/staging"))
    }
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { count_files(&p) } else { 1 }
        })
        .sum()
}

fn reference_of(bytes: &[u8]) -> String {
    format!("artifact://sha256/{:x}", Sha256::digest(bytes))
}

#[test]
fn a_named_file_is_stored_and_referenced() {
    let f = fixture("named");
    f.write("change.diff", b"--- a\n+++ b\n");
    let mut output = json!({
        "patch": {"$file": "change.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "change.diff"}],
        "note": "done"
    });
    f.gather(&mut output).expect("gathered");
    let reference = reference_of(b"--- a\n+++ b\n");
    assert_eq!(
        output,
        json!({"patch": reference, "more": [reference], "note": "done"})
    );
    // One blob, read once for both places it is named.
    assert_eq!(f.blobs(), 1);
}

/// Only where the contract expects a reference: a `$file` object anywhere
/// else is the output's own, and stays.
#[test]
fn a_file_object_where_no_reference_is_expected_is_left_alone() {
    let f = fixture("own");
    f.write("change.diff", b"x");
    let own = json!({"$file": "change.diff", "mediaType": "text/x-diff"});
    let mut output = json!({
        "patch": {"$file": "change.diff", "mediaType": "text/x-diff"},
        "meta": own.clone()
    });
    f.gather(&mut output).expect("gathered");
    assert_eq!(output["meta"], own, "the output's own object was taken");
    assert_eq!(output["patch"], json!(reference_of(b"x")));
    // Under a string field the object breaks the contract, and is not taken
    // for a file either.
    let mut output = json!({
        "patch": {"$file": "change.diff", "mediaType": "text/x-diff"},
        "note": {"$file": "change.diff"}
    });
    let e = f
        .gather(&mut output)
        .expect_err("a string field holds no file");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
}

/// The shape is checked before anything is stored: a rejected output leaves
/// no blob.
#[test]
fn a_rejected_output_stores_nothing() {
    let f = fixture("rejected");
    f.write("change.diff", b"x");
    for mut output in [
        // The contract refuses the output as a whole.
        json!({"patch": {"$file": "change.diff", "mediaType": "text/x-diff"}, "extra": 1}),
        // The media type is not the one the contract wants.
        json!({"patch": {"$file": "change.diff", "mediaType": "text/plain"}}),
        // A name that is not a string, a media type that is not one, and a
        // key besides the two.
        json!({"patch": {"$file": 7, "mediaType": "text/x-diff"}}),
        json!({"patch": {"$file": "change.diff", "mediaType": 7}}),
        json!({"patch": {"$file": "change.diff", "mediaType": "text/x-diff", "x": 1}}),
        // A path, not a name.
        json!({"patch": {"$file": "../change.diff", "mediaType": "text/x-diff"}}),
        json!({"patch": {"$file": "sub/change.diff", "mediaType": "text/x-diff"}}),
    ] {
        let before = output.clone();
        let e = f.gather(&mut output).expect_err("refused");
        assert!(matches!(e, ArtifactError::Refused(_)), "{before}: {e}");
        assert_eq!(f.blobs(), 0, "{before} stored a blob");
    }
}

/// Only a regular file of the command's own, through the directory held
/// open before it ran.
#[test]
fn links_fifos_directories_and_hard_links_are_refused() {
    let f = fixture("kinds");
    // Each its own outside file, so no other check stands in for the one
    // it tests: the link's target has one link, the hard link's two.
    let linked = f.dir.join("linked.txt");
    std::fs::write(&linked, b"not the command's").expect("outside");
    std::os::unix::fs::symlink(&linked, f.exchange.outputs.join("link")).expect("link");
    let outside = f.dir.join("outside.txt");
    std::fs::write(&outside, b"not the command's either").expect("outside");
    std::fs::hard_link(&outside, f.exchange.outputs.join("hard")).expect("hard link");
    std::fs::create_dir(f.exchange.outputs.join("dir")).expect("dir");
    let fifo = std::ffi::CString::new(f.exchange.outputs.join("fifo").to_str().expect("UTF-8"))
        .expect("path");
    // SAFETY: a NUL-terminated path, for one call.
    #[allow(unsafe_code)]
    let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo");
    for name in ["link", "hard", "dir", "fifo", "missing"] {
        let mut output = json!({"patch": {"$file": name, "mediaType": "text/x-diff"}});
        let e = f.gather(&mut output).expect_err(name);
        assert_eq!(f.blobs(), 0, "{name} stored a blob");
        assert!(matches!(e, ArtifactError::Refused(_)), "{name}: {e}");
    }
}

/// A command that swaps its outputs directory for a link to somewhere else
/// still hands over only what is in the directory it was given.
#[test]
fn a_swapped_outputs_directory_leads_nowhere() {
    let f = fixture("swapped");
    let elsewhere = f.dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::write(elsewhere.join("change.diff"), b"from elsewhere").expect("decoy");
    std::fs::rename(&f.exchange.outputs, f.dir.join("moved")).expect("move");
    std::os::unix::fs::symlink(&elsewhere, &f.exchange.outputs).expect("swap");
    let mut output = json!({"patch": {"$file": "change.diff", "mediaType": "text/x-diff"}});
    let e = f.gather(&mut output).expect_err("the decoy is not read");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    assert_eq!(f.blobs(), 0);
    // What is in the directory it was given is still what it hands over.
    std::fs::write(f.dir.join("moved/change.diff"), b"its own").expect("write");
    f.gather(&mut output).expect("gathered");
    assert_eq!(output["patch"], json!(reference_of(b"its own")));
}

#[test]
fn a_file_or_a_step_past_its_limit_is_refused() {
    let f = fixture("limits");
    f.write("a.diff", &[b'a'; 100]);
    f.write("b.diff", &[b'b'; 100]);
    let mut output = json!({"patch": {"$file": "a.diff", "mediaType": "text/x-diff"}});
    let e = f
        .gather_within(
            &mut output,
            Limits {
                per_file: 99,
                per_step: 1000,
            },
        )
        .expect_err("a file too large");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "b.diff"}]
    });
    let e = f
        .gather_within(
            &mut output,
            Limits {
                per_file: 100,
                per_step: 150,
            },
        )
        .expect_err("a step too large");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    // A name the output gives twice is read, and counted, once.
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "a.diff"}]
    });
    f.gather_within(
        &mut output,
        Limits {
            per_file: 100,
            per_step: 150,
        },
    )
    .expect("one file, named twice");
    // Sparse: the size is what counts, not the blocks it takes.
    let sparse = std::fs::File::create(f.exchange.outputs.join("sparse")).expect("sparse");
    sparse.set_len(1 << 40).expect("a terabyte, sparse");
    let mut output = json!({"patch": {"$file": "sparse", "mediaType": "text/x-diff"}});
    let e = f.gather(&mut output).expect_err("a sparse terabyte");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
}

/// Each referenced artifact, and only those the input contract types as
/// artifacts, is copied in, read-only, checked against its digest.
#[test]
fn inputs_are_copies_of_what_the_contract_references_read_only() {
    let f = fixture("inputs");
    let patch = f
        .store
        .put(&mut &b"the patch"[..], provenance())
        .expect("put")
        .artifact;
    let other = f
        .store
        .put(&mut &b"not an input"[..], provenance())
        .expect("put")
        .artifact;
    let input = json!({"patch": patch.to_string(), "note": other.to_string()});
    let given = referenced(&contract(), &input, &f.store);
    assert_eq!(given, vec![patch.clone()], "only the typed reference");
    f.exchange.mount(&f.store, &given).expect("mounted");
    let copy = f.exchange.inputs.join(patch.digest());
    assert_eq!(std::fs::read(&copy).expect("copy"), b"the patch");
    assert!(!f.exchange.inputs.join(other.digest()).exists());
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&copy), 0o444);
    assert_eq!(mode(&f.exchange.inputs), 0o555);
    // A copy is the command's own: writing it, were it allowed, leaves the
    // store as it was.
    assert_ne!(
        std::fs::canonicalize(&copy).expect("copy"),
        f.dir
            .join("store/blobs/sha256")
            .join(&patch.digest()[..2])
            .join(&patch.digest()[2..])
    );
}

#[test]
fn a_damaged_blob_is_not_given_to_a_command() {
    let f = fixture("damaged");
    let patch = f
        .store
        .put(&mut &b"the patch"[..], provenance())
        .expect("put")
        .artifact;
    let blob = f
        .dir
        .join("store/blobs/sha256")
        .join(&patch.digest()[..2])
        .join(&patch.digest()[2..]);
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::fs::write(&blob, b"the patch, changed").expect("damage");
    let e = f
        .exchange
        .mount(&f.store, std::slice::from_ref(&patch))
        .expect_err("refused");
    assert!(matches!(e, ArtifactError::Corrupt(_)), "{e}");
    assert!(!f.exchange.inputs.join(patch.digest()).exists());
}

/// A store that changes a file as it is about to read the `at`-th, counting
/// from one, after atomic opened it, as a process the command left running
/// might.
struct Tampering<'a> {
    inner: &'a LocalStore,
    path: PathBuf,
    change: fn(&Path),
    at: usize,
    stages: std::sync::atomic::AtomicUsize,
}

impl ArtifactStore for Tampering<'_> {
    fn stage(&self, content: &mut dyn Read) -> Result<crate::artifact::Staged, ArtifactError> {
        if self
            .stages
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
            == self.at
        {
            (self.change)(&self.path);
        }
        self.inner.stage(content)
    }
    fn commit(
        &self,
        staged: crate::artifact::Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        self.inner.commit(staged, provenance)
    }
    fn discard(&self, staged: crate::artifact::Staged) {
        self.inner.discard(staged)
    }
    fn hold(&self, artifact: &ArtifactRef) -> Result<Hold, ArtifactError> {
        self.inner.hold(artifact)
    }
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError> {
        self.inner.contains(artifact)
    }
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError> {
        self.inner.open(artifact)
    }
    fn manifests(
        &self,
        artifact: &ArtifactRef,
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        self.inner.manifests(artifact)
    }
}

/// A store that counts what it stages and commits, and fails the stage or
/// the commit of the given number, counting from one.
struct Counting<'a> {
    inner: &'a LocalStore,
    stages: std::sync::atomic::AtomicUsize,
    commits: std::sync::atomic::AtomicUsize,
    fail_stage: usize,
    fail_commit: usize,
}

impl<'a> Counting<'a> {
    fn new(inner: &'a LocalStore, fail_stage: usize, fail_commit: usize) -> Self {
        Self {
            inner,
            stages: Default::default(),
            commits: Default::default(),
            fail_stage,
            fail_commit,
        }
    }

    fn stages(&self) -> usize {
        self.stages.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn failed() -> ArtifactError {
    ArtifactError::Io {
        path: "counting".into(),
        source: std::io::Error::other("the store failed"),
    }
}

impl ArtifactStore for Counting<'_> {
    fn stage(&self, content: &mut dyn Read) -> Result<crate::artifact::Staged, ArtifactError> {
        let n = self
            .stages
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if n == self.fail_stage {
            return Err(failed());
        }
        self.inner.stage(content)
    }
    fn commit(
        &self,
        staged: crate::artifact::Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        let n = self
            .commits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if n == self.fail_commit {
            return Err(failed());
        }
        self.inner.commit(staged, provenance)
    }
    fn discard(&self, staged: crate::artifact::Staged) {
        self.inner.discard(staged)
    }
    fn hold(&self, artifact: &ArtifactRef) -> Result<Hold, ArtifactError> {
        self.inner.hold(artifact)
    }
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError> {
        self.inner.contains(artifact)
    }
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError> {
        self.inner.open(artifact)
    }
    fn manifests(
        &self,
        artifact: &ArtifactRef,
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        self.inner.manifests(artifact)
    }
}

impl Fixture {
    fn gather_through(
        &self,
        store: &dyn ArtifactStore,
        output: &mut Value,
    ) -> Result<Vec<Hold>, ArtifactError> {
        self.exchange.gather(
            store,
            &self.store,
            output,
            &contract(),
            &validate,
            &provenance(),
            Limits::default(),
        )
    }
}

/// A file that shrinks while it is read is refused, and nothing is stored;
/// one that grows is read up to the size it had when opened.
#[test]
fn a_file_that_changes_while_it_is_read_hands_over_only_what_it_had() {
    let f = fixture("changes");
    f.write("change.diff", b"0123456789");
    let shrinking = Tampering {
        inner: &f.store,
        path: f.exchange.outputs.join("change.diff"),
        change: |path| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_len(4))
                .expect("shrink");
        },
        // As it is read, after the first file.
        at: 2,
        stages: Default::default(),
    };
    // Read after another file, which is then not stored either.
    f.write("a.diff", b"first");
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "change.diff"}]
    });
    let e = f
        .gather_through(&shrinking, &mut output)
        .expect_err("refused");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    assert_eq!(f.blobs(), 0, "a shrunk file left a blob");
    assert_eq!(f.staged(), 0, "a shrunk file left a staged file");

    f.write("grow.diff", b"0123456789");
    let growing = Tampering {
        inner: &f.store,
        path: f.exchange.outputs.join("grow.diff"),
        change: |path| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(b"and more"))
                .expect("grow");
        },
        at: 1,
        stages: Default::default(),
    };
    let mut output = json!({"patch": {"$file": "grow.diff", "mediaType": "text/x-diff"}});
    f.gather_through(&growing, &mut output).expect("gathered");
    assert_eq!(output["patch"], json!(reference_of(b"0123456789")));
}

/// One refused file, of several, stores none of them.
#[test]
fn a_refusal_of_any_file_stores_none() {
    let f = fixture("any");
    f.write("a.diff", b"first");
    f.write("big.diff", &[b'b'; 200]);
    std::os::unix::fs::symlink(f.dir.join("store"), f.exchange.outputs.join("link")).expect("link");
    for (second, limits) in [
        ("link", Limits::default()),
        ("missing", Limits::default()),
        (
            "big.diff",
            Limits {
                per_file: 100,
                per_step: 1000,
            },
        ),
        (
            "big.diff",
            Limits {
                per_file: 1000,
                per_step: 150,
            },
        ),
    ] {
        let mut output = json!({
            "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
            "more": [{"$file": second}]
        });
        let e = f.gather_within(&mut output, limits).expect_err(second);
        assert!(matches!(e, ArtifactError::Refused(_)), "{second}: {e}");
        assert_eq!(f.blobs(), 0, "{second} left the first file stored");
    }
    // The output's own reference to an artifact the store does not hold.
    let missing = format!("artifact://sha256/{}", "0".repeat(64));
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [missing]
    });
    let e = f.gather(&mut output).expect_err("a missing reference");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    assert_eq!(f.blobs(), 0, "a missing reference left the file stored");
}

#[test]
fn stand_ins_cannot_be_forged_and_one_name_may_have_two_media_types() {
    let f = fixture("forged");
    f.write("a.diff", b"x");
    // An output that holds this check's stand-in is refused.
    let forged = stand_in("fixed", 0).to_string();
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "note": forged
    });
    let e = f
        .exchange
        .gather_with(
            &f.store,
            &f.store,
            &mut output,
            &contract(),
            &validate,
            &provenance(),
            Limits::default(),
            "fixed",
        )
        .expect_err("a forged stand-in");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
    assert_eq!(f.blobs(), 0);
    // One name under two media types: one blob, a manifest for each.
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "a.diff", "mediaType": "text/plain"}]
    });
    let counting = Counting::new(&f.store, 0, 0);
    f.gather_through(&counting, &mut output)
        .expect("two media types");
    assert_eq!(counting.stages(), 1, "the file was read once");
    let reference = reference_of(b"x");
    assert_eq!(output, json!({"patch": reference, "more": [reference]}));
    assert_eq!(f.blobs(), 1);
    let digest = reference.trim_start_matches("artifact://sha256/");
    let manifests = f
        .store
        .manifests(&ArtifactRef::from_digest(digest).expect("a reference"))
        .expect("manifests");
    let mut types: Vec<String> = manifests.into_iter().map(|m| m.media_type).collect();
    types.sort();
    assert_eq!(types, ["text/plain", "text/x-diff"]);
}

#[test]
fn an_output_naming_too_many_files_is_refused() {
    let f = fixture("many");
    f.write("a.diff", b"x");
    let more: Vec<Value> = (0..MAX_FILE_OBJECTS)
        .map(|_| json!({"$file": "a.diff"}))
        .collect();
    let mut output =
        json!({"patch": {"$file": "a.diff", "mediaType": "text/x-diff"}, "more": more});
    let e = f.gather(&mut output).expect_err("too many");
    assert!(matches!(e, ArtifactError::Refused(_)), "{e}");
}

/// Ten thousand objects naming one file: the check stays linear.
#[test]
fn many_objects_naming_one_file_are_gathered_quickly() {
    let f = fixture("one-many");
    f.write("a.diff", b"x");
    let more: Vec<Value> = (0..MAX_FILE_OBJECTS - 1)
        .map(|_| json!({"$file": "a.diff"}))
        .collect();
    let mut output =
        json!({"patch": {"$file": "a.diff", "mediaType": "text/x-diff"}, "more": more});
    let started = std::time::Instant::now();
    f.gather(&mut output).expect("gathered");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(f.blobs(), 1);
}

/// A store that fails to stage or to commit one of several files leaves
/// nothing staged.
#[test]
fn a_failed_stage_or_commit_leaves_nothing_staged() {
    let f = fixture("leak");
    f.write("a.diff", b"first");
    f.write("b.diff", b"second");
    f.write("c.diff", b"third");
    for (fail_stage, fail_commit) in [(2, 0), (3, 0), (0, 1), (0, 2)] {
        let failing = Counting::new(&f.store, fail_stage, fail_commit);
        let mut output = json!({
            "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
            "more": [{"$file": "b.diff"}, {"$file": "c.diff"}]
        });
        let e = f
            .gather_through(&failing, &mut output)
            .expect_err("the store failed");
        assert!(matches!(e, ArtifactError::Io { .. }), "{e}");
        assert_eq!(
            f.staged(),
            0,
            "stage {fail_stage} or commit {fail_commit} failing left staged files"
        );
    }
}

/// Past the most files one output may name, an output is refused before
/// any is read; up to it, each is read in turn.
#[test]
fn an_output_naming_more_files_than_the_most_is_refused() {
    let f = fixture("names");
    let names = 1_500;
    for i in 0..names {
        f.write(&format!("{i}.txt"), format!("file {i}").as_bytes());
    }
    let more: Vec<Value> = (0..names)
        .map(|i| json!({"$file": format!("{i}.txt")}))
        .collect();
    let mut output = json!({"patch": {"$file": "0.txt", "mediaType": "text/x-diff"}, "more": more});
    let e = f.gather(&mut output).expect_err("too many files");
    assert!(
        matches!(&e, ArtifactError::Refused(m) if m.contains("at most")),
        "{e}"
    );
    assert_eq!((f.blobs(), f.staged()), (0, 0));
    let more: Vec<Value> = (1..MAX_FILES)
        .map(|i| json!({"$file": format!("{i}.txt")}))
        .collect();
    let mut output = json!({"patch": {"$file": "0.txt", "mediaType": "text/x-diff"}, "more": more});
    f.gather(&mut output).expect("as many as the most");
    assert_eq!(f.blobs(), MAX_FILES);
}

/// `$file` objects where the contract expects no reference are the
/// output's own, and do not count toward the most an output may name.
#[test]
fn the_outputs_own_file_objects_are_not_counted() {
    let f = fixture("own-many");
    f.write("a.diff", b"x");
    let own: Vec<Value> = (0..=MAX_FILE_OBJECTS)
        .map(|_| json!({"$file": "elsewhere"}))
        .collect();
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "meta": {"files": own.clone()}
    });
    f.gather(&mut output).expect("gathered");
    assert_eq!(output["patch"], json!(reference_of(b"x")));
    assert_eq!(output["meta"]["files"], json!(own));
}

/// A store that sweeps itself after each commit as a sweep that began
/// reading what to keep long after the commit would, keeping nothing it was
/// told of and nothing recent, while a gather has more files to store and
/// its step has recorded none of them.
struct Sweeping<'a> {
    inner: &'a LocalStore,
}

impl Sweeping<'_> {
    fn sweep(&self) -> crate::artifact::Swept {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(24 * 3600);
        self.inner
            .sweep(
                &BTreeSet::new(),
                crate::artifact::Grace::none_for_tests(),
                later,
            )
            .expect("sweep")
    }
}

impl ArtifactStore for Sweeping<'_> {
    fn stage(&self, content: &mut dyn Read) -> Result<crate::artifact::Staged, ArtifactError> {
        self.inner.stage(content)
    }
    fn commit(
        &self,
        staged: crate::artifact::Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        let manifests = self.inner.commit(staged, provenance)?;
        self.sweep();
        Ok(manifests)
    }
    fn discard(&self, staged: crate::artifact::Staged) {
        self.inner.discard(staged)
    }
    fn hold(&self, artifact: &ArtifactRef) -> Result<Hold, ArtifactError> {
        self.inner.hold(artifact)
    }
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError> {
        self.inner.contains(artifact)
    }
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError> {
        self.inner.open(artifact)
    }
    fn manifests(
        &self,
        artifact: &ArtifactRef,
    ) -> Result<Vec<crate::artifact::Manifest>, ArtifactError> {
        self.inner.manifests(artifact)
    }
}

/// A sweep that lands while a gather stores its files, or after it and
/// before its step records them, keeps every file it stored, however long
/// the step takes: the gather holds them until they are let go. A sweep
/// after that removes them.
#[test]
fn a_sweep_racing_a_slow_gather_keeps_the_gathered_files() {
    let f = fixture("racing");
    f.write("a.diff", b"first");
    f.write("b.diff", b"second");
    let mut output = json!({
        "patch": {"$file": "a.diff", "mediaType": "text/x-diff"},
        "more": [{"$file": "b.diff"}]
    });
    let sweeping = Sweeping { inner: &f.store };
    let held = f
        .gather_through(&sweeping, &mut output)
        .expect("gathered while swept");
    let refs = [&output["patch"], &output["more"][0]];
    for reference in refs {
        let a: ArtifactRef = reference
            .as_str()
            .expect("a reference")
            .parse()
            .expect("parses");
        assert!(f.store.contains(&a).expect("contains"), "{reference} swept");
    }
    assert_eq!(held.len(), 2);
    // Long after, and still not recorded.
    assert!(sweeping.sweep().removed.is_empty());
    drop(held);
    assert_eq!(sweeping.sweep().removed.len(), 2);
}
