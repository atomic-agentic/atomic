// SPDX-License-Identifier: Apache-2.0

//! Refuses a build that would link a V8 archive other than the one
//! deps/v8.toml pins.
//!
//! rusty_v8's build script reads its prebuilt archive from RUSTY_V8_MIRROR,
//! which the workspace's .cargo/config.toml points at a mirror
//! scripts/v8-archive.sh verified. An environment that sets
//! RUSTY_V8_ARCHIVE, builds V8 from source, or names another mirror would
//! skip that check, so this build script repeats it: the mirror must be a
//! local directory, and the archive in it for this target must have its
//! pinned SHA-256.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

struct Archive {
    version: String,
    target: String,
    file: String,
    sha256: String,
}

fn archives(pins: &str) -> Vec<Archive> {
    let mut out = Vec::new();
    for block in pins.split("[[archive]]").skip(1) {
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
        out.push(Archive {
            version: field("version"),
            target: field("target"),
            file: field("file"),
            sha256: field("sha256"),
        });
    }
    out
}

fn fail(message: String) -> ! {
    panic!("atomic links only the V8 archive deps/v8.toml pins: {message}");
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let pins_path = manifest.join("../../deps/v8.toml");
    println!("cargo:rerun-if-changed={}", pins_path.display());
    for name in ["RUSTY_V8_ARCHIVE", "V8_FROM_SOURCE", "RUSTY_V8_MIRROR"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for name in ["RUSTY_V8_ARCHIVE", "V8_FROM_SOURCE"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            fail(format!(
                "{name} is set; unset it and use scripts/v8-archive.sh"
            ));
        }
    }
    let mirror = std::env::var("RUSTY_V8_MIRROR")
        .unwrap_or_else(|_| fail("RUSTY_V8_MIRROR is not set".into()));
    if mirror.contains("://") {
        fail(format!(
            "RUSTY_V8_MIRROR is {mirror}, not a local mirror scripts/v8-archive.sh filled"
        ));
    }
    let pins = std::fs::read_to_string(&pins_path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", pins_path.display())));
    let target = std::env::var("TARGET").unwrap_or_default();
    let mut checked = 0;
    for archive in archives(&pins).iter().filter(|a| a.target == target) {
        let path = Path::new(&mirror)
            .join(format!("v{}", archive.version))
            .join(&archive.file);
        println!("cargo:rerun-if-changed={}", path.display());
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != archive.sha256 {
            fail(format!(
                "{} has SHA-256 {digest}, not the pinned {}",
                path.display(),
                archive.sha256
            ));
        }
        checked += 1;
    }
    if checked == 0 {
        fail(format!(
            "no pinned archive for {target} is in {mirror}; run scripts/v8-archive.sh"
        ));
    }
}
