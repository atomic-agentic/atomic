// SPDX-License-Identifier: Apache-2.0

//! Files an exec atom's command hands over, and the artifacts it is given,
//! so a large result goes between steps by reference.
//!
//! Both live in the command's workspace, inside its scratch directory, the
//! one directory a confined command may write ([`Exchange`]):
//!
//! - its outputs directory (`ATOMIC_OUTPUTS`), where it writes a file it
//!   produces and names it in its output, where the contract expects an
//!   artifact reference, with an object of its own:
//!
//!   ```json
//!   {"patch": {"$file": "change.diff", "mediaType": "text/x-diff"}}
//!   ```
//!
//!   [`Exchange::gather`] stores each such file and puts its reference,
//!   `artifact://sha256/<digest>`, in the object's place, so the step's
//!   output, and every checkpoint that holds it, keeps only the reference;
//! - its inputs directory (`ATOMIC_INPUTS`), where [`Exchange::mount`] puts
//!   a copy of each artifact its input references where the input contract
//!   expects one, named by its digest. The directory is read-only to a
//!   confined command, and the copies are read-only too.
//!
//! The outputs directory is opened before the command runs and read through
//! that handle afterwards, so swapping it for a link leads nowhere. A file
//! is opened beneath it without following links, must be a regular file
//! with one link, is read up to the size it had when it was opened, and is
//! held to [`Limits`].

use super::{ArtifactError, ArtifactRef, ArtifactStore, Provenance};
use atomic_schema::artifact::ArtifactLookup;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// The key of the object a command uses to name a file it wrote.
pub const FILE_KEY: &str = "$file";
/// The media type a named file is stored under when the object gives none.
pub const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

fn refused(message: String) -> ArtifactError {
    ArtifactError::Refused(message)
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ArtifactError + '_ {
    move |source| ArtifactError::Io {
        path: path.to_owned(),
        source,
    }
}

/// How much a command may hand over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest file, in bytes.
    pub per_file: u64,
    /// The most, in bytes, across every file one step hands over.
    pub per_step: u64,
}

impl Default for Limits {
    /// One GiB a file and four a step.
    fn default() -> Self {
        Self {
            per_file: 1 << 30,
            per_step: 4 << 30,
        }
    }
}

/// A command's outputs and inputs directories, inside its scratch
/// directory, with the outputs directory held open.
#[derive(Debug)]
pub struct Exchange {
    /// Where the command writes the files it hands over.
    pub outputs: PathBuf,
    /// Where the command reads the artifacts it is given.
    pub inputs: PathBuf,
    /// The outputs directory, opened before the command ran.
    outputs_dir: std::fs::File,
}

/// Make `path` a new private directory (0700).
fn private_dir(path: &Path) -> Result<(), ArtifactError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io(path))
}

/// Open the directory at `path` itself, not what a link there names.
fn open_dir(path: &Path) -> Result<std::fs::File, ArtifactError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path).map_err(io(path))
}

impl Exchange {
    /// Make the two directories in `scratch` and open the outputs one, before
    /// the command runs.
    pub fn create(scratch: &Path) -> Result<Self, ArtifactError> {
        let (outputs, inputs) = (scratch.join("outputs"), scratch.join("inputs"));
        private_dir(&outputs)?;
        private_dir(&inputs)?;
        let outputs_dir = open_dir(&outputs)?;
        Ok(Self {
            outputs,
            inputs,
            outputs_dir,
        })
    }

    /// Put a copy of each of `artifacts` in the inputs directory, named by
    /// its digest and read-only, then make the directory read-only. Each copy
    /// is checked against its digest as it is made.
    pub fn mount(
        &self,
        store: &dyn ArtifactStore,
        artifacts: &[ArtifactRef],
    ) -> Result<(), ArtifactError> {
        for artifact in artifacts {
            let path = self.inputs.join(artifact.digest());
            if path.exists() {
                continue;
            }
            let mut content = store.open(artifact)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o400);
            }
            let mut file = options.open(&path).map_err(io(&path))?;
            if let Err(e) = std::io::copy(&mut content, &mut file) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(if e.kind() == std::io::ErrorKind::InvalidData {
                    ArtifactError::Corrupt(artifact.clone())
                } else {
                    io(&path)(e)
                });
            }
            set_mode(&path, 0o444)?;
        }
        set_mode(&self.inputs, 0o555)
    }

    /// Replace each `{"$file": …}` object in `output` that sits where
    /// `contract` expects an artifact reference with the reference of the
    /// file it names, stored in `store` with `provenance`. An object
    /// anywhere else is left as it is: it is the output's own.
    ///
    /// Before anything is stored, the output with stand-in references in
    /// those places must pass `validate`, and the media types its objects
    /// give must be ones the contract accepts, so a rejected output leaves
    /// no blob. Each name is read once.
    pub fn gather(
        &self,
        store: &dyn ArtifactStore,
        output: &mut Value,
        contract: &Value,
        validate: &dyn Fn(&Value) -> Result<(), String>,
        provenance: &Provenance,
        limits: Limits,
    ) -> Result<(), ArtifactError> {
        let mut candidates = Vec::new();
        named_files(output, String::new(), &mut candidates);
        if candidates.is_empty() {
            return Ok(());
        }
        // Each candidate stands in as a reference of its own; the contract
        // check says which stand where a reference does.
        let mut probe = output.clone();
        let stand_ins: BTreeMap<String, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, (pointer, _))| {
                let stand_in = stand_in(i);
                if let Some(slot) = probe.pointer_mut(pointer) {
                    *slot = Value::String(stand_in.to_string());
                }
                (stand_in.digest().to_owned(), i)
            })
            .collect();
        let lookup = StandIns {
            stand_ins: &stand_ins,
            candidates: &candidates,
            asked: std::sync::Mutex::new(Vec::new()),
        };
        let checked = atomic_schema::artifact::check_refs(contract, &probe, &lookup);
        let asked = lookup.asked.into_inner().unwrap_or_default();
        // Put back the objects that are the output's own.
        for (i, (pointer, original)) in candidates.iter().enumerate() {
            if !asked.contains(&i)
                && let Some(slot) = probe.pointer_mut(pointer)
            {
                *slot = original.clone();
            }
        }
        if asked.is_empty() {
            return Ok(());
        }
        let mut files = Vec::new();
        for &i in &asked {
            let (pointer, object) = &candidates[i];
            let (name, media) = file_object(object)
                .map_err(|why| refused(format!("{}: {why}", display_pointer(pointer))))?;
            files.push((i, name, media));
        }
        // A media type the contract does not accept, or any other break of
        // the contract by a stand-in, is the output's fault.
        if let Err(e) = checked
            && stand_ins
                .keys()
                .any(|digest| e.to_string().contains(digest.as_str()))
        {
            return Err(refused(e.to_string()));
        }
        validate(&probe).map_err(refused)?;

        let mut stored: BTreeMap<&str, String> = BTreeMap::new();
        let mut total = 0u64;
        for (i, name, media) in &files {
            let reference = match stored.get(name) {
                Some(reference) => reference.clone(),
                None => {
                    let reference =
                        self.store_file(store, name, media, provenance, limits, &mut total)?;
                    stored.insert(name, reference.clone());
                    reference
                }
            };
            if let Some(slot) = output.pointer_mut(&candidates[*i].0) {
                *slot = Value::String(reference);
            }
        }
        Ok(())
    }

    /// Store the file `name` in the outputs directory and return its
    /// reference, adding its size to `total`.
    fn store_file(
        &self,
        store: &dyn ArtifactStore,
        name: &str,
        media: &str,
        provenance: &Provenance,
        limits: Limits,
        total: &mut u64,
    ) -> Result<String, ArtifactError> {
        let path = self.outputs.join(name);
        let file = open_beneath(&self.outputs_dir, name).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => refused(format!(
                "`{FILE_KEY}` names {name:?}, a link: a command hands over regular files only"
            )),
            _ => io(&path)(e),
        })?;
        let meta = file.metadata().map_err(io(&path))?;
        if !meta.is_file() {
            return Err(refused(format!(
                "`{FILE_KEY}` names {name:?}, which is not a regular file"
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() != 1 {
                return Err(refused(format!(
                    "`{FILE_KEY}` names {name:?}, which has {} links: a command hands over \
                     files of its own only",
                    meta.nlink()
                )));
            }
        }
        let size = meta.len();
        if size > limits.per_file {
            return Err(refused(format!(
                "`{FILE_KEY}` names {name:?}, of {size} bytes: a file may be {} at most",
                limits.per_file
            )));
        }
        *total = total.saturating_add(size);
        if *total > limits.per_step {
            return Err(refused(format!(
                "the files the command hands over come to more than {} bytes, the most one \
                 step may",
                limits.per_step
            )));
        }
        // Up to the size it had when opened: what is appended after is not
        // read, and a file that shrank is refused.
        let mut counted = Counted {
            inner: file.take(size),
            read: 0,
        };
        let manifest = store.put(
            &mut counted,
            Provenance {
                media_type: media.to_owned(),
                ..provenance.clone()
            },
        )?;
        if counted.read != size {
            return Err(refused(format!(
                "`{FILE_KEY}` names {name:?}, which changed while it was read"
            )));
        }
        Ok(manifest.artifact.to_string())
    }
}

/// The artifacts `input` references where `contract` expects references, in
/// the order the check meets them, each once. A reference anywhere else, or
/// one the store does not hold, is not among them.
pub fn referenced(
    contract: &Value,
    input: &Value,
    lookup: &(dyn ArtifactLookup + Sync),
) -> Vec<ArtifactRef> {
    let recording = Recording {
        inner: lookup,
        asked: std::sync::Mutex::new(Vec::new()),
    };
    let _ = atomic_schema::artifact::check_refs(contract, input, &recording);
    let mut asked = recording.asked.into_inner().unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    asked.retain(|a| seen.insert(a.digest().to_owned()) && lookup.media_types(a).is_some());
    asked
}

/// A lookup that records what it was asked about.
struct Recording<'a> {
    inner: &'a (dyn ArtifactLookup + Sync),
    asked: std::sync::Mutex<Vec<ArtifactRef>>,
}

impl ArtifactLookup for Recording<'_> {
    fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(artifact.clone());
        }
        self.inner.media_types(artifact)
    }
}

/// A lookup that answers for the stand-in references, with the media type
/// each one's object gives, and records which the check asked about.
struct StandIns<'a> {
    stand_ins: &'a BTreeMap<String, usize>,
    candidates: &'a [(String, Value)],
    asked: std::sync::Mutex<Vec<usize>>,
}

impl ArtifactLookup for StandIns<'_> {
    fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>> {
        let &i = self.stand_ins.get(artifact.digest())?;
        if let Ok(mut asked) = self.asked.lock()
            && !asked.contains(&i)
        {
            asked.push(i);
        }
        let media = file_object(&self.candidates[i].1)
            .map(|(_, media)| media.to_owned())
            .unwrap_or_else(|_| DEFAULT_MEDIA_TYPE.to_owned());
        Some(vec![media])
    }
}

/// The reference that stands in for candidate `i` while the contract is
/// checked: a digest no content has.
fn stand_in(i: usize) -> ArtifactRef {
    let digest = format!("{:x}", Sha256::digest(format!("atomic stand-in {i}")));
    ArtifactRef::from_digest(&digest).expect("a SHA-256 digest is a reference")
}

/// Every object in `value` with a `$file` key, with its JSON Pointer.
fn named_files(value: &Value, pointer: String, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(fields) if fields.contains_key(FILE_KEY) => {
            out.push((pointer, value.clone()));
        }
        Value::Object(fields) => {
            for (key, field) in fields {
                named_files(field, format!("{pointer}/{}", escape(key)), out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                named_files(item, format!("{pointer}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// A key escaped for a JSON Pointer.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// A JSON Pointer as an error names it: `/` for the whole output.
fn display_pointer(pointer: &str) -> &str {
    if pointer.is_empty() { "/" } else { pointer }
}

/// The name and media type a `{"$file": name, "mediaType": type}` object
/// gives, or why it is not one: only those two keys, a name that is one
/// plain file name in the outputs directory, and a media type that is a
/// string.
fn file_object(value: &Value) -> Result<(&str, &str), String> {
    let object = value.as_object().ok_or("not an object")?;
    if let Some(key) = object.keys().find(|k| *k != FILE_KEY && *k != "mediaType") {
        return Err(format!(
            "an object naming a file has only `{FILE_KEY}` and `mediaType`, not `{key}`"
        ));
    }
    let name = match object.get(FILE_KEY) {
        Some(Value::String(name)) => name.as_str(),
        other => return Err(format!("`{FILE_KEY}` must be a file name, not {other:?}")),
    };
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(format!(
            "`{FILE_KEY}` names {name:?}: one file name in the outputs directory, with no `/`"
        ));
    }
    let media = match object.get("mediaType") {
        None => DEFAULT_MEDIA_TYPE,
        Some(Value::String(media)) if !media.is_empty() => media.as_str(),
        Some(other) => return Err(format!("`mediaType` must be a media type, not {other}")),
    };
    Ok((name, media))
}

/// Open `name` beneath `dir` for reading, not following a link, not waiting
/// on a FIFO, and not taking a terminal.
#[cfg(unix)]
fn open_beneath(dir: &std::fs::File, name: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name)?;
    // SAFETY: `dir` is an open directory for the whole call, `name` is a
    // NUL-terminated string that outlives it, and the flags ask for nothing
    // that writes.
    #[allow(unsafe_code)]
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    #[allow(unsafe_code)]
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn open_beneath(_dir: &std::fs::File, name: &str) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::other(format!(
        "{name}: handing files over is supported on Unix only"
    )))
}

/// Set `path`'s mode where modes exist.
fn set_mode(path: &Path, mode: u32) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io(path))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// A reader that counts what it read.
struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}
