// SPDX-License-Identifier: Apache-2.0

//! Files an exec atom's command hands over, and the artifacts it is given,
//! so a large result goes between steps by reference.
//!
//! A command is given two directories. It writes a file it produces into
//! its outputs directory and names it in its output, where the contract
//! expects an artifact, with an object of its own:
//!
//! ```json
//! {"patch": {"$file": "change.diff", "mediaType": "text/x-diff"}}
//! ```
//!
//! [`gather`] streams each such file into the store and puts its reference,
//! `artifact://sha256/<digest>`, in the object's place, before the output is
//! checked against its contract. The step's output, and every checkpoint
//! that holds it, keeps only the reference.
//!
//! For each artifact reference in a command's input, [`mount`] puts an entry
//! named by its digest in the command's inputs directory: a link to the
//! stored content where the store keeps it on disk, so nothing is copied,
//! else a copy streamed out of the store. Only what the input references is
//! there.

use super::{ArtifactError, ArtifactRef, ArtifactStore, Provenance};
use serde_json::Value;
use std::path::{Component, Path};

/// The key of the object a command uses to name a file it wrote.
pub const FILE_KEY: &str = "$file";
/// The media type a named file is stored under when the object gives none.
pub const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

fn refused(message: String) -> ArtifactError {
    ArtifactError::Refused(message)
}

/// The file `{"$file": name, "mediaType": type}` names, if `value` is such an
/// object.
fn named_file(value: &Value) -> Option<Result<(&str, &str), ArtifactError>> {
    let object = value.as_object()?;
    let name = object.get(FILE_KEY)?;
    if object.keys().any(|k| k != FILE_KEY && k != "mediaType") {
        return Some(Err(refused(format!(
            "an object naming a file has only `{FILE_KEY}` and `mediaType`, not {value}"
        ))));
    }
    let (Some(name), media) = (name.as_str(), object.get("mediaType")) else {
        return Some(Err(refused(format!(
            "`{FILE_KEY}` must be a file name, not {name}"
        ))));
    };
    let media = match media {
        None => DEFAULT_MEDIA_TYPE,
        Some(Value::String(m)) => m.as_str(),
        Some(other) => {
            return Some(Err(refused(format!(
                "`mediaType` must be a string, not {other}"
            ))));
        }
    };
    Some(Ok((name, media)))
}

/// Replace each `{"$file": …}` object in `value` with the reference of the
/// file it names under `dir`, stored in `store` with `provenance` and the
/// object's media type. A name must stay inside `dir` and name a regular
/// file there.
pub fn gather(
    store: &dyn ArtifactStore,
    dir: &Path,
    value: &mut Value,
    provenance: &Provenance,
) -> Result<(), ArtifactError> {
    if let Some(named) = named_file(value) {
        let (name, media) = named?;
        let path = inside(dir, name)?;
        let mut file = std::fs::File::open(&path).map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        let manifest = store.put(
            &mut file,
            Provenance {
                media_type: media.to_owned(),
                ..provenance.clone()
            },
        )?;
        *value = Value::String(manifest.artifact.to_string());
        return Ok(());
    }
    match value {
        Value::Array(items) => items
            .iter_mut()
            .try_for_each(|item| gather(store, dir, item, provenance)),
        Value::Object(fields) => fields
            .values_mut()
            .try_for_each(|field| gather(store, dir, field, provenance)),
        _ => Ok(()),
    }
}

/// `name` under `dir`, refused unless it is a plain relative path that ends
/// at a regular file inside `dir`, links followed.
fn inside(dir: &Path, name: &str) -> Result<std::path::PathBuf, ArtifactError> {
    let plain = Path::new(name)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if name.is_empty() || !plain {
        return Err(refused(format!(
            "`{FILE_KEY}` names {name:?}: a file name relative to the outputs directory, with \
             no `..`"
        )));
    }
    let root = std::fs::canonicalize(dir).map_err(|source| ArtifactError::Io {
        path: dir.to_owned(),
        source,
    })?;
    let path = std::fs::canonicalize(dir.join(name)).map_err(|source| ArtifactError::Io {
        path: dir.join(name),
        source,
    })?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(refused(format!(
            "`{FILE_KEY}` names {name:?}, which is not a file in the outputs directory"
        )));
    }
    Ok(path)
}

/// Make every artifact `value` references readable under `dir`, by digest.
/// A string that is not a reference is left alone; a reference to an
/// artifact the store does not hold is an error.
pub fn mount(store: &dyn ArtifactStore, value: &Value, dir: &Path) -> Result<(), ArtifactError> {
    match value {
        Value::String(text) => {
            let Ok(artifact) = text.parse::<ArtifactRef>() else {
                return Ok(());
            };
            let entry = dir.join(artifact.digest());
            if entry.exists() {
                return Ok(());
            }
            let io = |source| ArtifactError::Io {
                path: entry.clone(),
                source,
            };
            match store.local_path(&artifact) {
                #[cfg(unix)]
                Some(blob) => std::os::unix::fs::symlink(blob, &entry).map_err(io),
                #[cfg(not(unix))]
                Some(blob) => std::fs::copy(blob, &entry).map(|_| ()).map_err(io),
                None => {
                    if !store.contains(&artifact)? {
                        return Err(ArtifactError::Missing(artifact));
                    }
                    let mut from = store.open(&artifact)?;
                    let mut to = std::fs::File::create(&entry).map_err(io)?;
                    std::io::copy(&mut from, &mut to).map(|_| ()).map_err(io)
                }
            }
        }
        Value::Array(items) => items.iter().try_for_each(|item| mount(store, item, dir)),
        Value::Object(fields) => fields.values().try_for_each(|f| mount(store, f, dir)),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::artifact::LocalStore;
    use serde_json::json;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atomic-artifact-files-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["store", "outputs", "inputs", "elsewhere"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        dir
    }

    #[test]
    fn a_named_file_becomes_a_reference_and_is_mounted_for_the_next_command() {
        let dir = scratch("round");
        let store = LocalStore::open(dir.join("store")).unwrap();
        std::fs::write(dir.join("outputs/change.diff"), "+ a line\n").unwrap();
        let mut output = json!({"patch": {"$file": "change.diff", "mediaType": "text/x-diff"},
                                "note": "kept", "list": [{"$file": "change.diff"}]});
        gather(
            &store,
            &dir.join("outputs"),
            &mut output,
            &Provenance::default(),
        )
        .unwrap();
        let reference = output["patch"].as_str().unwrap().to_owned();
        assert!(reference.starts_with("artifact://sha256/"), "{output}");
        assert_eq!(output["note"], "kept");
        assert_eq!(
            output["list"][0],
            json!(reference),
            "the same content, once"
        );
        let artifact: ArtifactRef = reference.parse().unwrap();
        let types: Vec<String> = store
            .manifests(&artifact)
            .unwrap()
            .into_iter()
            .map(|m| m.media_type)
            .collect();
        assert!(types.contains(&"text/x-diff".to_owned()), "{types:?}");

        mount(&store, &json!({"patch": reference}), &dir.join("inputs")).unwrap();
        let entry = dir.join("inputs").join(artifact.digest());
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "+ a line\n");
        // Read in place, not copied.
        assert!(std::fs::symlink_metadata(&entry).unwrap().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_outside_the_outputs_directory_is_refused() {
        let dir = scratch("outside");
        let store = LocalStore::open(dir.join("store")).unwrap();
        std::fs::write(dir.join("elsewhere/secret"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere/secret"), dir.join("outputs/link")).unwrap();
        for name in ["../elsewhere/secret", "/etc/passwd", "link", "", "missing"] {
            let mut output = json!({"patch": {"$file": name}});
            let refused = gather(
                &store,
                &dir.join("outputs"),
                &mut output,
                &Provenance::default(),
            );
            assert!(refused.is_err(), "{name:?} was accepted");
        }
        let mut odd = json!({"patch": {"$file": "x", "extra": 1}});
        assert!(
            gather(
                &store,
                &dir.join("outputs"),
                &mut odd,
                &Provenance::default()
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reference_to_a_missing_artifact_is_not_mounted() {
        let dir = scratch("missing");
        let store = LocalStore::open(dir.join("store")).unwrap();
        let missing = format!("artifact://sha256/{}", "0".repeat(64));
        assert!(mount(&store, &json!([missing]), &dir.join("inputs")).is_err());
        // Strings that are not references are not artifacts.
        mount(
            &store,
            &json!({"text": "artifact? no"}),
            &dir.join("inputs"),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
