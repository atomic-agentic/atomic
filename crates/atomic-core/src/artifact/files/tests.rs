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

    fn gather(&self, output: &mut Value) -> Result<(), ArtifactError> {
        self.gather_within(output, Limits::default())
    }

    fn gather_within(&self, output: &mut Value, limits: Limits) -> Result<(), ArtifactError> {
        self.exchange.gather(
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
        if name != "missing" {
            assert!(matches!(e, ArtifactError::Refused(_)), "{name}: {e}");
        }
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
    assert!(matches!(e, ArtifactError::Io { .. }), "{e}");
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
