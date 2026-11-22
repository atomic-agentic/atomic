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
/// only write there, cannot sign a checkpoint. Without `ATOMIC_HOME` it
/// sits under [`crate::state::user_config_dir`] — the same directory the
/// person's own configuration resolves under and the denial is made of —
/// so one spelling of `HOME`, `XDG_CONFIG_HOME` or the password database
/// places settings, credentials, keys and the denial together, and never a
/// place a working directory would complete. A process running as the same
/// user can still read the key; it detects tampering by anything with less
/// access than that.
pub fn default_key_dir() -> Result<PathBuf, String> {
    match crate::state::atomic_home().map_err(|e| e.to_string())? {
        Some(home) => Ok(home.join("keys")),
        None => crate::state::user_config_dir()
            .map(|config| config.join("keys"))
            .ok_or_else(|| {
                "the home directory is unknown (HOME is unset or not an absolute path, and \
                 the user has no home in the password database), so there is no configuration \
                 directory for the context key; set HOME, or set ATOMIC_HOME"
                    .into()
            }),
    }
}

/// The file in the key directory that holds the checkpoint authentication
/// key.
const KEY_FILE: &str = "context.key";

/// [`default_key_dir`], for reading or making the key: refused while an
/// older atomic's key is stranded at its old place (see
/// [`crate::state::refuse_stranded`]), where making a new one would leave
/// the runs signed with the old one unable to resume. What the new place
/// holds is the key file alone, so the empty directory the denial makes
/// (see [`crate::codex::StateGuard`]) holds none.
fn default_key_dir_in_use() -> Result<PathBuf, String> {
    let dir = default_key_dir()?;
    if crate::state::atomic_home()
        .map_err(|e| e.to_string())?
        .is_none()
    {
        let legacy = crate::state::legacy_config_dir().map(|d| d.join("keys"));
        crate::state::refuse_stranded("checkpoint keys", &dir, legacy.as_deref(), Some(KEY_FILE))?;
    }
    Ok(dir)
}

fn authentication_key(program: &crate::astro::Program, key_dir: &Path) -> Result<Vec<u8>, String> {
    let dir = resolved(key_dir)?;
    if inside_source(&dir, program) {
        return Err("the context key must be outside the atom's source".into());
    }
    private_dir(&dir)?;
    let dir = std::fs::canonicalize(&dir).map_err(|e| e.to_string())?;
    let key_file = dir.join(KEY_FILE);
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

/// Resolve public context only if this run has no existing checkpoint,
/// authenticated with the key in the default key directory (see
/// [`default_key_dir`]), which is refused while an older atomic's key is
/// stranded at its old place. Settings and environment are read lazily,
/// after checking for a saved run.
pub fn checkpoint(
    atom: &Atom,
    state_dir: &Path,
    run_id: &str,
    resolve: impl FnOnce() -> Result<(Value, Value, Option<String>, Value), String>,
) -> Result<ContextSnapshot, String> {
    let keys = default_key_dir_in_use()?;
    checkpoint_with_key(atom, state_dir, &keys, run_id, resolve)
}

/// [`checkpoint`], with the key kept in `key_dir`.
fn checkpoint_with_key(
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

/// Where a workflow step's public context comes from: the settings and
/// profile its run was started with, and the project it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct DispatchSettings {
    /// Context settings, as `--context-settings` gives them.
    pub settings: Value,
    /// The configuration profile, if any.
    pub profile: Option<String>,
    /// The project's id and name, for `context.project`.
    pub project: Value,
}

/// Resolve a workflow step's public context from its run's settings. The
/// durable runtime records the result as a step of its own, so a retried
/// step restores it with [`ContextSnapshot::restore`] instead of reading
/// settings and environment again.
pub fn resolve_for_step(
    atom: &Atom,
    run_id: &str,
    step_id: &str,
    dispatch: &DispatchSettings,
) -> Result<ContextSnapshot, String> {
    resolve_fresh(atom, run_id, || {
        Ok((
            dispatch.settings.clone(),
            json!({"run": {"id": run_id}, "step": {"id": step_id}, "project": dispatch.project}),
            dispatch.profile.clone(),
            json!({}),
        ))
    })
}

impl ContextSnapshot {
    /// A snapshot the durable runtime recorded for `run_id`, checked again
    /// against the atom it was resolved for: the same compiled version, the
    /// same run, and the atom's full context contract.
    pub fn restore(atom: &Atom, run_id: &str, recorded: Value) -> Result<Self, String> {
        let snapshot = Self(recorded);
        snapshot.verify_for(&atom.program.hash(), run_id)?;
        validate_context(atom, snapshot.as_value())?;
        Ok(snapshot)
    }
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
        let snapshot = checkpoint_with_key(&atom, &state, &keys, "context-run", || {
            Ok((settings, metadata, Some("test".into()), json!({})))
        })
        .expect("checkpoint");
        let restored = checkpoint_with_key(&atom, &state, &keys, "context-run", || {
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
            step: None,
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
            checkpoint_with_key(&atom, &state, &keys, "another-run", || panic!(
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
            checkpoint_with_key(&atom, &state, &keys, "context-run", || panic!(
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

    #[cfg(unix)]
    #[test]
    fn migration_cannot_retarget_a_dangling_context_link_for_the_consumer() {
        let (root, atom) = contextual_atom();
        let home = crate::state::TestScratch::new("context-migration-link");
        let state = crate::state::ProjectState::under(&home, &root);
        let old = state.dir().join("private");
        std::fs::create_dir_all(&old).expect("old layout");
        std::fs::write(old.join("atomic.db"), "old database").expect("old database");
        let outside = home.join("outside");
        std::fs::create_dir(&outside).expect("outside private state");
        std::os::unix::fs::symlink("../../outside", old.join("context")).expect("old link");
        assert_eq!(
            std::fs::canonicalize(old.join("context"))
                .expect_err("dangling at source")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        let migrated = state.create();
        if migrated.is_ok() {
            // Exercise the actual consumer before the refusal assertion: the old
            // implementation admits the retargeted path and reaches resolution.
            assert_eq!(
                resolved(&state.context_dir()).expect("consumer resolves link"),
                outside
            );
            let error = checkpoint_with_key(
                &atom,
                &state.context_dir(),
                &home.join("keys"),
                "run",
                || Err("consumer reached outside private state".into()),
            )
            .expect_err("resolution sentinel");
            assert_eq!(error, "consumer reached outside private state");
        }
        match migrated.expect_err("migration must refuse the retargetable source link") {
            crate::state::StateError::Create { path, source } => {
                assert_eq!(path, old.join("context"));
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(source.to_string(), "legacy state entry is a symbolic link");
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(old.join("atomic.db")).expect("database remains"),
            "old database"
        );
        assert_eq!(
            std::fs::read_link(old.join("context")).expect("link remains"),
            Path::new("../../outside")
        );
        assert_eq!(
            std::fs::symlink_metadata(state.context_dir())
                .expect_err("no installed link")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            std::fs::read_dir(&outside)
                .expect("outside untouched")
                .count(),
            0
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_key_is_kept_apart_from_the_checkpoints() {
        let (root, atom) = contextual_atom();
        let state = root.with_extension("apart-state");
        let unreachable = || -> Result<(Value, Value, Option<String>, Value), String> {
            Err("resolution is not reached".into())
        };
        let error = checkpoint_with_key(&atom, &state, &state.join("keys"), "run", unreachable)
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
        let error =
            checkpoint_with_key(&atom, &visible, &keys, "run", unreachable).expect_err("visible");
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
            let error =
                checkpoint_with_key(&atom, &link.join("new-state"), &keys, "run", unreachable)
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
        let error =
            checkpoint_with_key(&atom, &hidden, &keys, "run", unreachable).expect_err("unresolved");
        assert_eq!(error, "resolution is not reached");
        // In a project every directory under the root is source.
        std::fs::write(root.join("atomic.yaml"), "atomic: 1\nname: test\n").expect("manifest");
        let error = checkpoint_with_key(&atom, &root.join(".state"), &keys, "run", unreachable)
            .expect_err("project");
        assert!(error.contains("outside source"), "{error}");
        let _ = std::fs::remove_dir_all(&keys);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The key directory is placed by the one resolution settings and the
    /// denial are made of: a relative `HOME` gives way to the home the
    /// password database gives, `XDG_CONFIG_HOME` moves the place, a
    /// relative one of those gives way to `HOME`, `ATOMIC_HOME` keeps the
    /// keys inside the home, and no absolute spelling at all is refused
    /// saying the home is unknown. The environment and the password
    /// database are what the test says they are, so this needs neither
    /// this machine's user nor this process's own environment; see
    /// [`crate::state::as_process`].
    #[test]
    fn the_key_directory_is_placed_by_the_one_configuration_directory() {
        /// The throwaway directory of the case, which goes when the case
        /// ends, however it ends.
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let made = std::env::temp_dir().join(format!(
            "atomic-keys-place-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&made);
        for dir in ["home", "settings", "real-home/.config/atomic"] {
            std::fs::create_dir_all(made.join(dir)).expect("a directory of the case");
        }
        let scratch =
            Scratch(std::fs::canonicalize(&made).expect("the throwaway directory placed"));
        let _relative_home =
            crate::state::as_process(&scratch.0, &[("HOME", "rel-home")], Some("real-home"));
        let config = scratch.0.join("real-home/.config/atomic");
        assert_eq!(
            default_key_dir(),
            Ok(config.join("keys")),
            "the key directory is not the one under the home the password database gives"
        );
        assert_eq!(
            default_key_dir().expect("keys"),
            crate::state::user_config_dir()
                .expect("the configuration directory")
                .join("keys"),
            "the key directory is not where settings and the denial are made"
        );
        drop(_relative_home);
        let _home = crate::state::as_process(&scratch.0, &[("HOME", "/home")], None);
        assert_eq!(
            default_key_dir(),
            Ok(scratch.0.join("home/.config/atomic/keys")),
            "the key directory is not under `HOME`"
        );
        drop(_home);
        let _xdg = crate::state::as_process(
            &scratch.0,
            &[("HOME", "/home"), ("XDG_CONFIG_HOME", "/settings")],
            None,
        );
        assert_eq!(
            default_key_dir(),
            Ok(scratch.0.join("settings/atomic/keys")),
            "`XDG_CONFIG_HOME` did not move the key directory"
        );
        drop(_xdg);
        let _relative_xdg = crate::state::as_process(
            &scratch.0,
            &[("HOME", "/home"), ("XDG_CONFIG_HOME", "settings")],
            None,
        );
        assert_eq!(
            default_key_dir(),
            Ok(scratch.0.join("home/.config/atomic/keys")),
            "a relative `XDG_CONFIG_HOME` did not give way to `HOME`"
        );
        drop(_relative_xdg);
        let _atomic = crate::state::as_process(
            &scratch.0,
            &[("HOME", "/home"), ("ATOMIC_HOME", "/home")],
            None,
        );
        assert_eq!(
            default_key_dir(),
            Ok(scratch.0.join("home/keys")),
            "`ATOMIC_HOME` did not keep the keys inside the home"
        );
        drop(_atomic);
        let _none = crate::state::as_process(&scratch.0, &[("HOME", "rel-home")], None);
        let why = default_key_dir().expect_err("no absolute spelling of a home");
        assert!(
            why.starts_with("the home directory is unknown") && why.contains("set ATOMIC_HOME"),
            "not the unknown-home refusal for the key directory: {why}"
        );
    }

    /// A run's checkpoint is authenticated with the key in the default key
    /// directory, and while an older atomic's key is still at its old place
    /// and the new place holds none it is refused, naming both, rather than
    /// signed with a new key — also once the denial has made the new
    /// directory, empty, as every run with a Codex or exec atom does before
    /// its first checkpoint. No key is made there; once the old key is moved
    /// in, it is the one used.
    #[test]
    fn a_checkpoint_is_refused_while_an_older_atomics_key_is_stranded() {
        let (root, atom) = contextual_atom();
        let scratch = crate::state::TestScratch::new("stranded-key");
        for dir in ["home", "legacy-config/atomic/keys", "project-state"] {
            std::fs::create_dir_all(scratch.join(dir)).expect("a directory of the case");
        }
        let legacy = scratch.join("legacy-config/atomic/keys");
        std::fs::write(legacy.join(KEY_FILE), [7u8; 32]).expect("the older atomic's key");
        let _process = crate::state::as_process_with_dirs(
            &scratch,
            &[("HOME", "/home")],
            None,
            Some("legacy-config"),
            Some("data"),
        );
        let _guard = crate::codex::StateGuard::deny_state_dir(scratch.join("project-state"))
            .expect("the guard");
        let new = scratch.join("home/.config/atomic/keys");
        assert_eq!(
            std::fs::read_dir(&new).map(|entries| entries.count()).ok(),
            Some(0),
            "the guard did not make the new key directory, empty"
        );
        let unreachable = || -> Result<(Value, Value, Option<String>, Value), String> {
            Err("resolution is not reached".into())
        };
        let checkpoints = scratch.join("checkpoints");
        let why = checkpoint(&atom, &checkpoints, "run", unreachable).expect_err("stranded");
        assert_eq!(
            why,
            format!(
                "checkpoint keys are kept in {new} now, which holds none of them, and an older \
                 atomic kept them in {legacy}, which still holds them: move everything in \
                 {legacy} into {new}, making {new} first if it does not exist, to keep using \
                 them, or set ATOMIC_HOME to a directory that holds them in its `keys` directory",
                new = new.display(),
                legacy = legacy.display(),
            )
        );
        assert_eq!(
            std::fs::symlink_metadata(new.join(KEY_FILE))
                .err()
                .map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound),
            "a new key was made beside the stranded one"
        );
        std::fs::rename(legacy.join(KEY_FILE), new.join(KEY_FILE)).expect("moved");
        let why = checkpoint(&atom, &checkpoints, "run", unreachable).expect_err("unresolved");
        assert_eq!(
            why, "resolution is not reached",
            "the moved key was not used"
        );
        assert_eq!(
            std::fs::read(new.join(KEY_FILE)).ok(),
            Some(vec![7u8; 32]),
            "the moved key was replaced"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
