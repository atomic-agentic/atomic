// SPDX-License-Identifier: Apache-2.0
//! Fingerprints the exact authoring implementation used for durable
//! snapshots, and refuses a build that would link a V8 archive other than the
//! one deps/v8.toml pins.
//!
//! rusty_v8's build script reads its prebuilt archive from RUSTY_V8_MIRROR,
//! which scripts/dev.sh and CI point at a mirror scripts/v8-archive.sh
//! verified. This build script repeats that check
//! and refuses the rusty_v8 settings that would read or link anything else
//! (see `build/v8_archive.rs`).
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[path = "build/v8_archive.rs"]
mod v8_archive;

fn check_v8_archive() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let pins_path = manifest.join("../../deps/v8.toml");
    let lock_path = manifest.join("../../Cargo.lock");
    for path in [&pins_path, &lock_path] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for name in v8_archive::REFUSED.iter().chain(v8_archive::READ) {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let read = |path: &Path| {
        std::fs::read_to_string(path).unwrap_or_else(|e| {
            panic!(
                "atomic links only the V8 archive deps/v8.toml pins: cannot read {}: {e}",
                path.display()
            )
        })
    };
    let env = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    let target = std::env::var("TARGET").unwrap_or_default();
    match v8_archive::check(&env, &read(&pins_path), &read(&lock_path), &target) {
        Ok(read) => {
            for path in read {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        Err(message) => panic!("atomic links only the V8 archive deps/v8.toml pins: {message}"),
    }
}

fn identity(name: &str, files: &[&str]) {
    let mut digest = Sha256::new();
    for file in [
        "Cargo.toml",
        "../../Cargo.lock",
        // The change carried in the vendored deno_core and deno_v8, which
        // Cargo.lock does not record.
        "../../third_party/deno/pr-36665.diff",
        "src/buffer_budget.rs",
        "src/child_exit.rs",
        "src/windows_worker.rs",
    ]
    .into_iter()
    .chain(files.iter().copied())
    {
        println!("cargo:rerun-if-changed={file}");
        let bytes = std::fs::read(Path::new(file)).expect("authoring build input");
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    // The vendored deno_core and deno_v8, whose sources Cargo.lock does not
    // checksum: every file, by its path in the tree and its bytes.
    for tree in [
        "../../third_party/deno/deno_core",
        "../../third_party/deno/deno_v8",
    ] {
        println!("cargo:rerun-if-changed={tree}");
        let mut files = Vec::new();
        walk(Path::new(tree), &mut files);
        files.sort();
        for file in files {
            let bytes = std::fs::read(&file).expect("vendored build input");
            let relative = file.strip_prefix(tree).expect("a path in the tree");
            // `/`-separated on every host, so the identity is the same.
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            digest.update((relative.len() as u64).to_le_bytes());
            digest.update(relative.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
    }
    println!("cargo:rustc-env={name}={:x}", digest.finalize());
}

/// Every regular file under `dir`; anything else fails the build.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("vendored source directory") {
        let path = entry.expect("vendored source entry").path();
        let kind = std::fs::symlink_metadata(&path)
            .expect("vendored source metadata")
            .file_type();
        if kind.is_dir() {
            walk(&path, files);
        } else if kind.is_file() {
            files.push(path);
        } else {
            panic!("{} is neither a file nor a directory", path.display());
        }
    }
}
fn main() {
    check_v8_archive();
    identity(
        "ATOMIC_COMPILER_HASH",
        &[
            "src/compiler_host.rs",
            "src/compiler_process.rs",
            "src/compiler_host/host.js",
            "src/compiler_host/astro_parser.rs",
            "src/compiler_host/typescript.rs",
            "src/compiler_host/limits.rs",
            "assets/worker.mjs",
            "assets/typescript-es2022.json",
        ],
    );
    identity(
        "ATOMIC_HELPER_HASH",
        &[
            "src/helper_runtime.rs",
            "src/helper_runtime/process.rs",
            "src/helper_runtime/zone.rs",
            "src/child_memory.rs",
            "src/worker_lifetime.rs",
            "src/astro.rs",
            "src/astro/windows_paths.rs",
        ],
    );
}
