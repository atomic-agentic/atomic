// SPDX-License-Identifier: Apache-2.0

//! Frozen public execution context for Astro dispatch.
//! The file checkpoint serves direct invocation; workflow persistence belongs
//! to the durable runtime and must not be implemented as a nested atom step.

use crate::atom::Atom;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// A host-created or authenticated checkpoint. Untrusted JSON cannot construct it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ContextSnapshot(Value);

impl ContextSnapshot {
    /// Public prompt-visible values; this accessor cannot mutate the snapshot.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub(crate) fn verify_for(&self, program_hash: &str, run_id: &str) -> Result<(), String> {
        if self.0["runId"] != run_id {
            return Err("context run identity mismatch".into());
        }
        if self.0["programHash"] != program_hash {
            return Err("context compiled atom version mismatch".into());
        }
        Ok(())
    }
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}

fn mac(snapshot: &Value, key: &[u8]) -> Result<Hmac<Sha256>, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(b"atomic-context-v1\0");
    mac.update(&serde_json::to_vec(&canonical(snapshot)).map_err(|e| e.to_string())?);
    Ok(mac)
}

// Exclusive publication: racing writers use the winning complete file.
fn publish(state_dir: &Path, file: &Path, bytes: &[u8]) -> Result<(), String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = state_dir.join(format!(
        ".context-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut candidate = options.open(&temporary).map_err(|e| e.to_string())?;
        candidate.write_all(bytes).map_err(|e| e.to_string())?;
        candidate.sync_all().map_err(|e| e.to_string())?;
        match std::fs::hard_link(&temporary, file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
        std::fs::File::open(state_dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

/// `path` made absolute with every existing part's symlinks resolved, without
/// creating anything, so a check made on it holds for the directory created later.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        let name = existing
            .file_name()
            .ok_or("context state directory has no existing ancestor")?;
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .ok_or("context state directory has no existing ancestor")?;
    }
    let mut out = std::fs::canonicalize(existing).map_err(|e| e.to_string())?;
    for name in missing.into_iter().rev() {
        if Path::new(&name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("context state directory must not contain `..`".into());
        }
        out.push(name);
    }
    Ok(out)
}

/// Whether runtime state at `dir` would sit in the atom's source tree.
///
/// In a project, everything under the project root is source. A standalone
/// atom's root is just its directory, which may be the home directory, so
/// there state may sit under a hidden directory (such as `~/.local/share`),
/// but never in the root itself, in a visible folder or around a source file.
fn inside_source(dir: &Path, program: &crate::astro::Program) -> bool {
    let root = &program.root;
    let project = root.join("atomic.yaml").is_file() || root.join(".atomic/atomic.yaml").is_file();
    let Ok(relative) = dir.strip_prefix(root) else {
        return false;
    };
    if project || relative.as_os_str().is_empty() {
        return true;
    }
    let hidden = relative
        .components()
        .next()
        .is_some_and(|part| part.as_os_str().to_string_lossy().starts_with('.'));
    !hidden
        || program
            .sources
            .keys()
            .any(|file| Path::new(file).starts_with(dir))
}

fn private_dir(dir: &Path) -> Result<(), String> {
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// The default directory for the checkpoint authentication key: apart from
/// the checkpoints, so a copy of the state directory, or a process that can
/// only write there, cannot sign a checkpoint. A process running as the same
/// user can still read the key; it detects tampering by anything with less
/// access than that.
pub fn default_key_dir() -> Result<PathBuf, String> {
    match std::env::var_os("ATOMIC_HOME").filter(|value| !value.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join("keys")),
        None => dirs::config_dir()
            .map(|dir| dir.join("atomic/keys"))
            .ok_or_else(|| {
                "no user configuration directory for the context key; set ATOMIC_HOME".into()
            }),
    }
}

fn authentication_key(program: &crate::astro::Program, key_dir: &Path) -> Result<Vec<u8>, String> {
    let dir = resolved(key_dir)?;
    if inside_source(&dir, program) {
        return Err("the context key must be outside the atom's source".into());
    }
    private_dir(&dir)?;
    let dir = std::fs::canonicalize(&dir).map_err(|e| e.to_string())?;
    let key_file = dir.join("context.key");
    if !key_file.exists() {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| e.to_string())?;
        publish(&dir, &key_file, &key)?;
    }
    let key = std::fs::read(&key_file).map_err(|e| e.to_string())?;
    if key.len() != 32 {
        return Err("invalid context authentication key".into());
    }
    Ok(key)
}

/// Resolve public context only if this run has no existing checkpoint.
/// Settings and environment are read lazily, after checking for a saved run.
pub fn checkpoint(
    atom: &Atom,
    state_dir: &Path,
    key_dir: &Path,
    run_id: &str,
    resolve: impl FnOnce() -> Result<(Value, Value, Option<String>, Value), String>,
) -> Result<ContextSnapshot, String> {
    if run_id.is_empty() || run_id.len() > 256 {
        return Err("invalid run id".into());
    }
    // Check where the directory would be before creating any of it.
    let state_dir = resolved(state_dir)?;
    if inside_source(&state_dir, &atom.program) {
        return Err(format!(
            "context state must be outside source: {} is inside {}",
            state_dir.display(),
            atom.program.root.display()
        ));
    }
    let key_dir = resolved(key_dir)?;
    if key_dir.starts_with(&state_dir) || state_dir.starts_with(&key_dir) {
        return Err("the context key must not be kept with the checkpoints".into());
    }
    private_dir(&state_dir)?;
    let state_dir = std::fs::canonicalize(&state_dir).map_err(|e| e.to_string())?;
    if inside_source(&state_dir, &atom.program) {
        return Err("context state must be outside source".into());
    }
    let key = authentication_key(&atom.program, &key_dir)?;
    let hash = format!("{:x}", Sha256::digest(run_id.as_bytes()));
    let file = state_dir.join(format!("{hash}.json"));
    let load = || -> Result<ContextSnapshot, String> {
        let mut bytes = Vec::new();
        std::fs::File::open(&file)
            .map_err(|e| e.to_string())?
            .take(8_388_609)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 8_388_608 {
            return Err("context checkpoint size limit exceeded".into());
        }
        let envelope: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let tag: Vec<u8> = serde_json::from_value(envelope["authentication"].clone())
            .map_err(|_| "context checkpoint requires authentication")?;
        mac(&envelope["snapshot"], &key)?
            .verify_slice(&tag)
            .map_err(|_| "context checkpoint authentication failed")?;
        let snapshot = ContextSnapshot(envelope["snapshot"].clone());
        snapshot.verify_for(&atom.program.hash(), run_id)?;
        validate_context(atom, snapshot.as_value())?;
        Ok(snapshot)
    };
    match std::fs::metadata(&file) {
        Ok(_) => return load(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let snapshot = resolve_fresh(atom, run_id, resolve)?;
    let authentication = mac(snapshot.as_value(), &key)?
        .finalize()
        .into_bytes()
        .to_vec();
    publish(
        &state_dir,
        &file,
        &serde_json::to_vec(&json!({"snapshot":snapshot,"authentication":authentication}))
            .map_err(|e| e.to_string())?,
    )?;
    load()
}

fn validate_context(atom: &Atom, snapshot: &Value) -> Result<(), String> {
    let schema = atom
        .context
        .as_ref()
        .ok_or("atom has no context contract")?;
    jsonschema::validator_for(schema)
        .map_err(|e| e.to_string())?
        .validate(&snapshot["context"])
        .map_err(|e| e.to_string())
}

/// Resolve an ephemeral preview against current settings without creating a checkpoint.
pub fn resolve_fresh(
    atom: &Atom,
    run_id: &str,
    resolve: impl FnOnce() -> Result<(Value, Value, Option<String>, Value), String>,
) -> Result<ContextSnapshot, String> {
    if run_id.is_empty() || run_id.len() > 256 {
        return Err("invalid run id".into());
    }
    let (settings, metadata, profile, overrides) = resolve()?;
    let schema = atom
        .context
        .as_ref()
        .ok_or("atom has no context contract")?;
    let mut environment = serde_json::Map::new();
    let entries = settings
        .get("environment")
        .and_then(Value::as_object)
        .ok_or("context settings need an environment mapping")?;
    // Reject all secret declarations before the first environment read.
    for (key, entry) in entries {
        if entry.get("secret").is_some_and(|value| value != false) {
            return Err(format!(
                "secret values cannot enter prompt-visible context: {key} is declared secret, \
                 so no environment variable was read"
            ));
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or("invalid environment name")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        {
            return Err("invalid environment name".into());
        }
    }
    for entry in entries.values() {
        let name = entry["name"].as_str().ok_or("invalid environment name")?;
        match std::env::var(name) {
            Ok(value) => {
                environment.insert(name.into(), json!(value));
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let name = settings
        .get("schema")
        .and_then(Value::as_str)
        .ok_or("context settings need a schema name")?;
    let document =
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","$defs":{name:schema}});
    let mut resolution =
        json!({"document":document,"settings":settings,"metadata":metadata,"overrides":overrides});
    if let Some(profile) = profile {
        resolution["profile"] = json!(profile);
    }
    let snapshot = atom.program.invoke("resolveContext", json!({"resolution":resolution,"environment":environment,"programHash":atom.program.hash()}))?;
    let snapshot = ContextSnapshot(snapshot);
    snapshot.verify_for(&atom.program.hash(), run_id)?;
    validate_context(atom, snapshot.as_value())?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{atom, compile, render};

    #[test]
    fn production_context_reuses_its_checkpoint_rebinds_steps_and_rejects_tampering() {
        let root = atom::tests::project("{input.text}", "{}", None);
        let document: Value = serde_json::from_str(include_str!(
            "../../../authoring/schemas/contracts.schema.json"
        ))
        .expect("schema");
        fn expand(value: &Value, document: &Value) -> Value {
            if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
                return expand(
                    document
                        .pointer(reference.trim_start_matches('#'))
                        .expect("ref"),
                    document,
                );
            }
            match value {
                Value::Object(values) => Value::Object(
                    values
                        .iter()
                        .map(|(key, value)| (key.clone(), expand(value, document)))
                        .collect(),
                ),
                Value::Array(values) => {
                    Value::Array(values.iter().map(|value| expand(value, document)).collect())
                }
                _ => value.clone(),
            }
        }
        let mut schema = expand(&document["$defs"]["RuntimeContext"], &document);
        schema["properties"]["env"]["required"] = json!([]);
        std::fs::write(root.join("test.context.schema.json"), schema.to_string()).expect("schema");
        std::fs::write(
            root.join("test.atom.astro"),
            r#"---
import type { Props, Input, Output, Case, Context } from './.atomic/types/test';
import type { AtomConfig } from './.atomic/types/authoring';
const { input, context }: Props = Astro.props;
export const config: AtomConfig<Input, Output, Case, {}, Context> = {
  validate: [({output, context}) => output.ticket.length <= context.config.maxReplyCharacters],
};
---
Profile: {context.profile}; step: {context.step.id}; input: {input.text}
"#,
        )
        .expect("source");
        let mut registry = atomic_schema::registry::Registry::new();
        let atom = atom::load(&root.join("test.atom.astro"), &mut registry).expect("load");
        let state = root.with_extension("context-state");
        let keys = root.with_extension("context-keys");
        let mut settings: Value = serde_json::from_str(include_str!(
            "../../../authoring/fixtures/context/settings.json"
        ))
        .expect("settings");
        settings["environment"] = json!({});
        let metadata = json!({"run":{"id":"context-run"},"step":{"id":"draft"},"project":{"id":"example","name":"Example"}});
        let snapshot = checkpoint(&atom, &state, &keys, "context-run", || {
            Ok((settings, metadata, Some("test".into()), json!({})))
        })
        .expect("checkpoint");
        let restored = checkpoint(&atom, &state, &keys, "context-run", || {
            panic!("must not read live settings or environment")
        })
        .expect("restore");
        assert_eq!(snapshot, restored);
        let compiled = compile::compile(&atom, &registry).expect("compile");
        let input = json!({"text":"request"});
        let run = render::RunInput {
            run_id: "context-run",
            input: &input,
            frontmatter: &atom.frontmatter,
            origin: "external",
            origins: None,
        };
        let result = render::render_with_context(&compiled, &run, Some(&restored), Some("review"))
            .expect("dispatch");
        assert!(result.user.contains("origin=\"team\">review"));
        assert!(result.user.contains("origin=\"external\">request"));
        assert!(
            restored
                .verify_for(&atom.program.hash(), "another-run")
                .expect_err("cross-run")
                .contains("run identity")
        );
        let file = state.join(format!("{:x}.json", Sha256::digest(b"context-run")));
        let other_file = state.join(format!("{:x}.json", Sha256::digest(b"another-run")));
        std::fs::copy(&file, &other_file).expect("copy authentic checkpoint across runs");
        assert!(
            checkpoint(&atom, &state, &keys, "another-run", || panic!(
                "restore only"
            ))
            .expect_err("cross-run checkpoint")
            .contains("run identity")
        );
        let output = json!({"ticket":"ABC-1"});
        let case = json!({});
        let evaluation = crate::evaluation::Evaluation {
            run_id: "another-run",
            input: &input,
            output: &output,
            case: &case,
            snapshot: Some(&restored),
            step_id: Some("review"),
        };
        assert!(
            crate::evaluation::judge_input(&atom, &registry, "unused", &evaluation)
                .expect_err("cross-run judge input")
                .contains("run identity")
        );
        assert!(
            crate::evaluation::judge_score(
                &atom,
                "unused",
                &output,
                Some(&restored),
                Some("review"),
                "another-run"
            )
            .expect_err("cross-run judge score")
            .contains("run identity")
        );
        let mut envelope: Value =
            serde_json::from_slice(&std::fs::read(&file).expect("checkpoint file"))
                .expect("envelope");
        envelope["snapshot"]["context"]["profile"] = json!("prod");
        // Recompute the public checksum, as an attacker editing a saved file can.
        let object = envelope["snapshot"].as_object_mut().expect("snapshot");
        object.remove("digest");
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&canonical(&envelope["snapshot"])).expect("canonical")
            )
        );
        envelope["snapshot"]["digest"] = json!(digest);
        std::fs::write(&file, serde_json::to_vec(&envelope).expect("encode")).expect("tamper");
        assert!(
            checkpoint(&atom, &state, &keys, "context-run", || panic!(
                "restore only"
            ))
            .expect_err("tampered")
            .contains("authentication")
        );
        std::fs::remove_dir_all(state).expect("cleanup");
        std::fs::remove_dir_all(keys).expect("cleanup");
    }

    fn contextual_atom() -> (std::path::PathBuf, Atom) {
        let root = atom::tests::project("{input.text}", "{}", None);
        let schema = json!({"type":"object","additionalProperties":false,"required":["profile"],"properties":{"profile":{"type":"string"}}});
        std::fs::write(root.join("test.context.schema.json"), schema.to_string()).expect("schema");
        let mut registry = atomic_schema::registry::Registry::new();
        let atom = atom::load(&root.join("test.atom.astro"), &mut registry).expect("load");
        (root, atom)
    }

    #[test]
    fn a_secret_environment_value_never_enters_context() {
        let (root, atom) = contextual_atom();
        let settings = json!({
            "schema": "Context",
            "defaultProfile": "dev",
            "base": {},
            "profiles": {"dev": {}},
            "environment": {"TOKEN": {"name": "PATH", "required": false, "secret": true}}
        });
        let error = resolve_fresh(&atom, "run", || Ok((settings, json!({}), None, json!({}))))
            .expect_err("secret");
        // atomic refuses it before reading any environment variable; the
        // interpreter's own refusal, later, does not name the declaration.
        assert!(error.contains("TOKEN is declared secret"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_key_is_kept_apart_from_the_checkpoints() {
        let (root, atom) = contextual_atom();
        let state = root.with_extension("apart-state");
        let unreachable = || -> Result<(Value, Value, Option<String>, Value), String> {
            Err("resolution is not reached".into())
        };
        let error = checkpoint(&atom, &state, &state.join("keys"), "run", unreachable)
            .expect_err("key inside the state directory");
        assert!(
            error.contains("not be kept with the checkpoints"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&state);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn state_inside_source_is_refused_before_anything_is_created() {
        let (root, atom) = contextual_atom();
        let keys = root.with_extension("refused-keys");
        let unreachable = || -> Result<(Value, Value, Option<String>, Value), String> {
            Err("resolution is not reached".into())
        };
        // A visible folder in a standalone atom's directory is source.
        let visible = root.join("state/context");
        let error = checkpoint(&atom, &visible, &keys, "run", unreachable).expect_err("visible");
        assert!(error.contains("outside source"), "{error}");
        assert!(
            !root.join("state").exists(),
            "nothing is created before the check"
        );
        // A symlink from outside into the source is resolved before the check.
        #[cfg(unix)]
        {
            let link = root.with_extension("link-into-source");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&root, &link).expect("symlink");
            let error = checkpoint(&atom, &link.join("new-state"), &keys, "run", unreachable)
                .expect_err("symlinked");
            assert!(error.contains("outside source"), "{error}");
            assert!(
                !root.join("new-state").exists(),
                "nothing is created through the link"
            );
            std::fs::remove_file(link).expect("cleanup");
        }
        // A hidden directory under a standalone root, as `~/.local/share` is
        // under a home-directory atom, is not source.
        let hidden = root.join(".local/share/atomic/context");
        let error = checkpoint(&atom, &hidden, &keys, "run", unreachable).expect_err("unresolved");
        assert_eq!(error, "resolution is not reached");
        // In a project every directory under the root is source.
        std::fs::write(root.join("atomic.yaml"), "atomic: 1\nname: test\n").expect("manifest");
        let error = checkpoint(&atom, &root.join(".state"), &keys, "run", unreachable)
            .expect_err("project");
        assert!(error.contains("outside source"), "{error}");
        let _ = std::fs::remove_dir_all(&keys);
        let _ = std::fs::remove_dir_all(&root);
    }
}
