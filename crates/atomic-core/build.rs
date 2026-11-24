// SPDX-License-Identifier: Apache-2.0
//! Fingerprint the exact authoring implementation used for durable snapshots.
use sha2::{Digest, Sha256};
use std::path::Path;

fn identity(name: &str, files: &[&str]) {
    let mut digest = Sha256::new();
    for file in [
        "Cargo.toml",
        "../../Cargo.lock",
        "src/js_modules.rs",
        "src/js_budget.rs",
    ]
    .into_iter()
    .chain(files.iter().copied())
    {
        println!("cargo:rerun-if-changed={file}");
        let bytes = std::fs::read(Path::new(file)).expect("authoring build input");
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    println!("cargo:rustc-env={name}={:x}", digest.finalize());
}
fn main() {
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
        &["src/helper_runtime.rs", "src/helper_runtime/process.rs"],
    );
}
