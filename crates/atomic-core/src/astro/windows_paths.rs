// SPDX-License-Identifier: Apache-2.0
//! The embedded compiler uses POSIX module names. Keep filesystem access,
//! persisted source keys and diagnostics under their native Windows paths.
use super::path_text;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

const PROJECT: &str = "/project";

pub(super) struct Project(PathBuf);

impl Project {
    pub(super) fn new(root: &str) -> Self {
        Self(PathBuf::from(root))
    }

    fn module(&self, file: &str) -> Result<String, String> {
        let relative = Path::new(file)
            .strip_prefix(&self.0)
            .map_err(|_| format!("{file} is outside the compiler project"))?;
        let mut name = PROJECT.to_owned();
        for c in relative.components() {
            let Component::Normal(part) = c else {
                return Err(format!("invalid compiler source path {file}"));
            };
            name.push('/');
            name.push_str(part.to_str().ok_or("source path must be UTF-8")?);
        }
        Ok(name)
    }

    fn native(&self, name: &str) -> Result<String, String> {
        if name == PROJECT {
            return Ok(path_text(&self.0)?.to_owned());
        }
        let relative = name
            .strip_prefix("/project/")
            .ok_or_else(|| format!("{name} is outside the compiler project"))?;
        let mut path = self.0.clone();
        for part in relative.split('/') {
            if part.contains(['\\', ':'])
                || !matches!(
                    Path::new(part).components().next(),
                    Some(Component::Normal(_))
                )
            {
                return Err(format!("invalid compiler source path {name}"));
            }
            path.push(part);
        }
        Ok(path_text(&path)?.to_owned())
    }

    pub(super) fn request(&self, req: &mut Value) -> Result<(), String> {
        req["file"] = self
            .module(req["file"].as_str().ok_or("missing compiler file")?)?
            .into();
        req["root"] = PROJECT.into();
        for field in ["sources", "sourcePaths", "declarations"] {
            if let Some(map) = req.get_mut(field) {
                map_paths(map, |key| self.module(key), field == "sourcePaths")?;
            }
        }
        list_paths(req, "absent", |key| self.module(key))
    }

    pub(super) fn response(&self, action: &str, response: &mut Value) -> Result<(), String> {
        if let Some(error) = response.get_mut("error") {
            let translate = |e: &mut Value| -> Result<(), String> {
                if let Some(file) = e.get("file").and_then(Value::as_str)
                    && file.starts_with("/project/")
                {
                    e["file"] = self.native(file)?.into();
                }
                Ok(())
            };
            translate(error)?;
            if let Some(details) = error.get_mut("diagnostics").and_then(Value::as_array_mut) {
                for detail in details {
                    translate(detail)?;
                }
            }
        }
        if let Some(result) = response.get_mut("ok") {
            if action == "load" {
                for field in ["sources", "sourcePaths"] {
                    if let Some(map) = result.get_mut(field) {
                        map_paths(map, |key| self.native(key), field == "sourcePaths")?;
                    }
                }
                for field in ["needsSources", "typeImports"] {
                    list_paths(result, field, |key| self.native(key))?;
                }
            } else if action == "check" {
                map_paths(result, |key| self.native(key), false)?;
            }
        }
        Ok(())
    }
}

// Only protocol fields that name files are translated. Project text and model
// input/output can themselves contain path-like strings.
fn map_paths(
    value: &mut Value,
    translate: impl Fn(&str) -> Result<String, String>,
    aliases: bool,
) -> Result<(), String> {
    let map = value.as_object_mut().ok_or("invalid compiler source map")?;
    for (key, mut v) in std::mem::take(map) {
        if aliases {
            v = translate(v.as_str().ok_or("invalid compiler alias")?)?.into();
        }
        if map.insert(translate(&key)?, v).is_some() {
            return Err(format!("duplicate compiler source {key}"));
        }
    }
    Ok(())
}

fn list_paths(
    object: &mut Value,
    field: &str,
    translate: impl Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    if let Some(value) = object.get_mut(field) {
        for item in value.as_array_mut().ok_or("invalid compiler path list")? {
            *item = translate(item.as_str().ok_or("invalid compiler path")?)?.into();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocol_paths_round_trip_without_rewriting_source_text() {
        let root = std::env::current_dir()
            .expect("current directory")
            .join("atom project");
        let file = root.join("atoms").join("example.atom.astro");
        let file = path_text(&file).expect("UTF-8");
        let paths = Project(root.clone());
        let mut req = json!({"file": file, "root": root, "sources": {file: file}, "sourcePaths": {file: file}});
        paths.request(&mut req).expect("encode project");
        let name = "/project/atoms/example.atom.astro";
        assert_eq!(req["file"], name);
        assert_eq!(req["sources"][name], file);
        assert_eq!(req["sourcePaths"][name], name);

        let mut rsp = json!({"ok": {"sources": req["sources"], "sourcePaths": req["sourcePaths"], "needsSources": [name]}});
        paths.response("load", &mut rsp).expect("decode sources");
        assert_eq!(rsp["ok"]["sources"][file], file);
        assert_eq!(rsp["ok"]["sourcePaths"][file], file);
        assert_eq!(rsp["ok"]["needsSources"][0], file);

        for bad in [
            "/other/secret",
            "/project/../secret",
            "/project/a\\b",
            "/project/C:/secret",
        ] {
            let mut response = json!({"ok": {"needsSources": [bad]}});
            assert!(
                paths
                    .response("load", &mut response)
                    .expect_err("outside project")
                    .contains("compiler")
            );
        }
    }
}
