// SPDX-License-Identifier: Apache-2.0
//! The check atomic-core's build script makes before rusty_v8 links V8: the
//! build reads V8 only from a local mirror holding the archive and bindings
//! deps/v8.toml pins for the version Cargo.lock builds, and rusty_v8 is
//! given nothing that would make it read or link anything else.
//!
//! rusty_v8 reads its prebuilt archive and generated bindings from
//! `RUSTY_V8_MIRROR`. Its other settings name another archive, another
//! bindings file, another tag directory in the mirror, a network fallback, a
//! leftover library, or skip V8 altogether; each is refused. A mirror given
//! as a URL template or a remote URL is refused, and so is a copy at the
//! mirror's top level, which rusty_v8 would read when the tagged copy is
//! missing. `RUSTY_V8_ARCHIVE_SHA256` must be the pinned archive hash, so
//! rusty_v8 itself checks the copy it takes and any library a previous build
//! left. Compiled into the build script, and into atomic-core's tests.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// rusty_v8 settings a build refuses when they are set at all.
pub(crate) const REFUSED: &[&str] = &[
    "RUSTY_V8_ARCHIVE",
    "V8_FROM_SOURCE",
    "RUSTY_V8_MIRROR_TAG",
    "RUSTY_V8_MIRROR_FALLBACK",
    "RUSTY_V8_SKIP_DOWNLOAD",
    "RUSTY_V8_SRC_BINDING_PATH",
    "RUSTY_V8_SRC_BINDING_URL",
    "DOCS_RS",
    "DENO_TRYBUILD",
];

/// The rusty_v8 settings the check reads, which the build reruns on.
#[cfg_attr(
    test,
    expect(dead_code, reason = "only the build script reruns on them")
)]
pub(crate) const READ: &[&str] = &["RUSTY_V8_MIRROR", "RUSTY_V8_ARCHIVE_SHA256"];

/// One archive deps/v8.toml pins.
pub(crate) struct Pin {
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) file: String,
    pub(crate) sha256: String,
    pub(crate) binding: String,
    pub(crate) binding_sha256: String,
}

/// The `[[archive]]` entries of deps/v8.toml.
pub(crate) fn pins(text: &str) -> Vec<Pin> {
    let mut out = Vec::new();
    for block in text.split("[[archive]]").skip(1) {
        let field = |name: &str| {
            block
                .lines()
                .take_while(|line| !line.trim_start().starts_with("[["))
                .find_map(|line| {
                    let (key, value) = line.split_once('=')?;
                    (key.trim() == name).then(|| value.trim().trim_matches('"').to_owned())
                })
                .unwrap_or_default()
        };
        out.push(Pin {
            version: field("version"),
            target: field("target"),
            file: field("file"),
            sha256: field("sha256"),
            binding: field("binding"),
            binding_sha256: field("binding_sha256"),
        });
    }
    out
}

/// The versions of the `v8` package Cargo.lock holds, sorted and distinct.
pub(crate) fn locked_v8_versions(lock: &str) -> Vec<String> {
    let mut versions: Vec<String> = lock
        .split("[[package]]")
        .skip(1)
        .filter_map(|block| {
            let mut lines = block.lines().map(str::trim).filter(|l| !l.is_empty());
            if lines.next()? != "name = \"v8\"" {
                return None;
            }
            Some(
                lines
                    .next()?
                    .strip_prefix("version = ")?
                    .trim_matches('"')
                    .to_owned(),
            )
        })
        .collect();
    versions.sort();
    versions.dedup();
    versions
}

/// Whether a mirror names a remote location rather than a directory.
fn remote(mirror: &str) -> bool {
    mirror.starts_with("http:") || mirror.starts_with("https:") || mirror.contains("://")
}

/// Whether `path` exists. Only a true absence counts as absent.
fn present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

/// Checks the environment `env` gives rusty_v8 and the mirror it names,
/// against deps/v8.toml (`pins`), Cargo.lock (`lock`) and the target triple.
/// Returns the files the check read, for the build to rerun when they
/// change.
pub(crate) fn check(
    env: &dyn Fn(&str) -> Option<String>,
    pins_text: &str,
    lock: &str,
    target: &str,
) -> Result<Vec<PathBuf>, String> {
    for name in REFUSED {
        if env(name).is_some() {
            return Err(format!(
                "{name} is set, which makes rusty_v8 read or link something other than the pinned mirror; unset it"
            ));
        }
    }
    let mirror = env("RUSTY_V8_MIRROR")
        .filter(|mirror| !mirror.is_empty())
        .ok_or("RUSTY_V8_MIRROR is not set; scripts/dev.sh and .cargo/config.toml set it")?;
    if mirror.contains('{') {
        return Err(format!(
            "RUSTY_V8_MIRROR is {mirror}, a URL template, not the local mirror scripts/v8-archive.sh fills"
        ));
    }
    if remote(&mirror) {
        return Err(format!(
            "RUSTY_V8_MIRROR is {mirror}, a remote mirror, not the local one scripts/v8-archive.sh fills"
        ));
    }
    let versions = locked_v8_versions(lock);
    if versions.len() != 1 {
        return Err(format!(
            "Cargo.lock holds v8 versions {versions:?}, not exactly one"
        ));
    }
    let version = &versions[0];
    let all = pins(pins_text);
    let pin = all
        .iter()
        .find(|pin| pin.version == *version && pin.target == target)
        .ok_or_else(|| format!("deps/v8.toml pins no V8 {version} archive for {target}"))?;
    let pinned_sha = env("RUSTY_V8_ARCHIVE_SHA256").unwrap_or_default();
    if !pinned_sha.eq_ignore_ascii_case(&pin.sha256) {
        return Err(format!(
            "RUSTY_V8_ARCHIVE_SHA256 is {pinned_sha:?}, not the pinned {} for {target}; scripts/v8-archive.sh --sha256 prints it",
            pin.sha256
        ));
    }
    let mirror = Path::new(&mirror);
    let mut read = Vec::new();
    for (file, want) in [
        (&pin.file, &pin.sha256),
        (&pin.binding, &pin.binding_sha256),
    ] {
        let path = mirror.join(format!("v{version}")).join(file);
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "cannot read {}: {error}; run scripts/v8-archive.sh",
                path.display()
            )
        })?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != *want {
            return Err(format!(
                "{} has SHA-256 {digest}, not the pinned {want}",
                path.display()
            ));
        }
        read.push(path);
        // rusty_v8 also looks for the file at the mirror's top level.
        let flat = mirror.join(file);
        if present(&flat)? {
            return Err(format!(
                "{} is a copy rusty_v8 could read instead of the pinned one; remove it",
                flat.display()
            ));
        }
        read.push(flat);
    }
    Ok(read)
}
