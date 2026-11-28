// SPDX-License-Identifier: Apache-2.0
//! The build script's V8 archive check (`v8_archive.rs`), against a small
//! mirror in a scratch directory: each refusal, and the pinned case it
//! refuses everything but.
use crate::v8_archive::check;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const ARCHIVE: &[u8] = b"pinned archive";
const BINDING: &[u8] = b"pinned binding";
const LOCK: &str = "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"v8\"\nversion = \"152.2.0\"\nsource = \"git+https://example.invalid/rusty_v8.git\"\n";

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn pins() -> String {
    format!(
        "[[archive]]\ncrate = \"v8\"\nversion = \"152.2.0\"\ntarget = \"{TARGET}\"\nfile = \"lib.a.gz\"\nsha256 = \"{}\"\nbinding = \"binding.rs\"\nbinding_sha256 = \"{}\"\n",
        sha(ARCHIVE),
        sha(BINDING)
    )
}

/// A mirror holding the pinned files, and the environment a pinned build has.
struct Fixture {
    mirror: PathBuf,
    env: BTreeMap<String, String>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let mirror =
            std::env::temp_dir().join(format!("atomic-v8-archive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&mirror);
        std::fs::create_dir_all(mirror.join("v152.2.0")).expect("mirror");
        std::fs::write(mirror.join("v152.2.0/lib.a.gz"), ARCHIVE).expect("archive");
        std::fs::write(mirror.join("v152.2.0/binding.rs"), BINDING).expect("binding");
        let env = BTreeMap::from([
            (
                "RUSTY_V8_MIRROR".to_owned(),
                mirror.to_string_lossy().into_owned(),
            ),
            ("RUSTY_V8_ARCHIVE_SHA256".to_owned(), sha(ARCHIVE)),
        ]);
        Self { mirror, env }
    }

    fn set(mut self, name: &str, value: &str) -> Self {
        self.env.insert(name.to_owned(), value.to_owned());
        self
    }

    fn check_with(&self, lock: &str) -> Result<Vec<PathBuf>, String> {
        let env = |name: &str| self.env.get(name).cloned();
        check(&env, &pins(), lock, TARGET)
    }

    fn check(&self) -> Result<Vec<PathBuf>, String> {
        self.check_with(LOCK)
    }

    fn refusal(&self) -> String {
        self.check().expect_err("the check refuses")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.mirror);
    }
}

#[test]
fn the_pinned_mirror_passes() {
    let fixture = Fixture::new("pinned");
    let read = fixture.check().expect("the pinned mirror passes");
    assert!(read.contains(&fixture.mirror.join("v152.2.0/lib.a.gz")));
    assert!(read.contains(&fixture.mirror.join("v152.2.0/binding.rs")));
}

/// Each rusty_v8 setting that would read or link something other than the
/// pinned mirror is refused when it is set, even to an empty value.
fn refuses(name: &str) {
    for value in ["1", ""] {
        let error = Fixture::new(&name.to_lowercase())
            .set(name, value)
            .refusal();
        assert!(
            error.starts_with(&format!("{name} is set,")),
            "{name}={value:?}: {error}"
        );
    }
}

#[test]
fn refuses_an_archive_override() {
    refuses("RUSTY_V8_ARCHIVE");
}

#[test]
fn refuses_a_build_from_source() {
    refuses("V8_FROM_SOURCE");
}

#[test]
fn refuses_a_mirror_tag() {
    refuses("RUSTY_V8_MIRROR_TAG");
}

#[test]
fn refuses_a_mirror_fallback() {
    refuses("RUSTY_V8_MIRROR_FALLBACK");
}

#[test]
fn refuses_skipping_the_download() {
    refuses("RUSTY_V8_SKIP_DOWNLOAD");
}

// A bindings path is accepted only as the pinned bindings in the mirror,
// through any spelling of that path; any other file, a missing one or an
// empty value is refused.
#[test]
fn refuses_a_binding_path_but_the_pinned_one() {
    for spelling in ["v152.2.0/binding.rs", "v152.2.0/../v152.2.0/binding.rs"] {
        let fixture = Fixture::new("binding-path-ok");
        let given = fixture.mirror.join(spelling).to_string_lossy().into_owned();
        let fixture = fixture.set("RUSTY_V8_SRC_BINDING_PATH", &given);
        assert!(fixture.check().is_ok(), "{given}: {:?}", fixture.check());
    }
    let fixture = Fixture::new("binding-path");
    let other = fixture.mirror.join("other.rs");
    std::fs::write(&other, BINDING).expect("a copy with the pinned bytes");
    for given in [
        other.to_string_lossy().into_owned(),
        String::new(),
        "/nonexistent/binding.rs".into(),
    ] {
        let error = Fixture::new("binding-path-other")
            .set("RUSTY_V8_SRC_BINDING_PATH", &given)
            .refusal();
        assert!(
            error.starts_with(&format!(
                "RUSTY_V8_SRC_BINDING_PATH is {given:?}, not the pinned bindings"
            )),
            "{given}: {error}"
        );
    }
}

#[test]
fn refuses_a_binding_url() {
    refuses("RUSTY_V8_SRC_BINDING_URL");
}

#[test]
fn refuses_a_docs_build() {
    refuses("DOCS_RS");
}

#[test]
fn refuses_a_trybuild_build() {
    refuses("DENO_TRYBUILD");
}

#[test]
fn refuses_a_missing_mirror() {
    let mut fixture = Fixture::new("unset");
    fixture.env.remove("RUSTY_V8_MIRROR");
    assert!(fixture.refusal().starts_with("RUSTY_V8_MIRROR is not set"));
}

#[test]
fn refuses_a_url_template_mirror() {
    let fixture = Fixture::new("template");
    let template = format!("{}/{{tag}}/{{file}}", fixture.mirror.display());
    let error = fixture.set("RUSTY_V8_MIRROR", &template).refusal();
    assert!(error.contains("a URL template"), "{error}");
}

#[test]
fn refuses_a_remote_mirror() {
    for mirror in ["https:example.invalid/v8", "http:mirror", "file:///v8"] {
        let error = Fixture::new("remote")
            .set("RUSTY_V8_MIRROR", mirror)
            .refusal();
        assert!(error.contains("a remote mirror"), "{mirror}: {error}");
    }
}

#[test]
fn takes_the_version_from_the_lockfile() {
    let fixture = Fixture::new("version");
    let other = LOCK.replace("152.2.0", "150.4.0");
    let error = fixture.check_with(&other).expect_err("refused");
    assert_eq!(
        error,
        format!("deps/v8.toml pins no V8 150.4.0 archive for {TARGET}")
    );
    let two = format!("{LOCK}\n{other}");
    let error = fixture.check_with(&two).expect_err("refused");
    assert_eq!(
        error,
        r#"Cargo.lock holds v8 versions ["150.4.0", "152.2.0"], not exactly one"#
    );
}

#[test]
fn refuses_an_unpinned_archive_hash() {
    let error = Fixture::new("sha")
        .set("RUSTY_V8_ARCHIVE_SHA256", &sha(b"another archive"))
        .refusal();
    assert!(
        error.starts_with("RUSTY_V8_ARCHIVE_SHA256 is ")
            && error.contains(&format!("not the pinned {}", sha(ARCHIVE))),
        "{error}"
    );
    let mut fixture = Fixture::new("sha-unset");
    fixture.env.remove("RUSTY_V8_ARCHIVE_SHA256");
    assert!(
        fixture
            .refusal()
            .starts_with("RUSTY_V8_ARCHIVE_SHA256 is \"\""),
        "unset"
    );
}

#[test]
fn refuses_a_tampered_archive() {
    let fixture = Fixture::new("archive");
    std::fs::write(fixture.mirror.join("v152.2.0/lib.a.gz"), b"tampered").expect("tamper");
    let error = fixture.refusal();
    assert!(
        error.contains("lib.a.gz has SHA-256") && error.contains(&sha(ARCHIVE)),
        "{error}"
    );
}

#[test]
fn refuses_a_tampered_binding() {
    let fixture = Fixture::new("binding");
    std::fs::write(fixture.mirror.join("v152.2.0/binding.rs"), b"tampered").expect("tamper");
    let error = fixture.refusal();
    assert!(
        error.contains("binding.rs has SHA-256") && error.contains(&sha(BINDING)),
        "{error}"
    );
}

#[test]
fn refuses_a_copy_at_the_mirror_top_level() {
    for file in ["lib.a.gz", "binding.rs"] {
        let fixture = Fixture::new("flat");
        std::fs::write(fixture.mirror.join(file), b"another copy").expect("flat copy");
        let error = fixture.refusal();
        assert!(
            error.contains(&format!("{file} is a copy rusty_v8 could read")),
            "{error}"
        );
    }
}

// An unreadable mirror is refused, not taken as missing files: a directory
// the check cannot enter, and an archive that is a link to itself.
#[cfg(unix)]
#[test]
fn an_unreadable_mirror_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("loop");
    let archive = fixture.mirror.join("v152.2.0/lib.a.gz");
    std::fs::remove_file(&archive).expect("archive");
    std::os::unix::fs::symlink(&archive, &archive).expect("self link");
    let error = fixture.refusal();
    assert!(
        error.starts_with(&format!("cannot read {}", archive.display())),
        "{error}"
    );

    // A tag directory the check cannot read; under root, which reads it
    // anyway, this half fails rather than passing vacuously.
    let fixture = Fixture::new("mode");
    let tagged = fixture.mirror.join("v152.2.0");
    std::fs::set_permissions(&tagged, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let result = fixture.check();
    std::fs::set_permissions(&tagged, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let error = result.expect_err("refused");
    assert!(
        error.starts_with(&format!(
            "cannot read {}",
            tagged.join("lib.a.gz").display()
        )) && error.contains("ermission denied"),
        "{error}"
    );
}
