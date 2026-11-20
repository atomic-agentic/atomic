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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(all(test, unix))]
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
    /// anywhere else is left as it is: it is the output's own. `lookup` is
    /// where the output's other references are checked.
    ///
    /// Nothing is stored unless everything holds: the output with stand-in
    /// references in those places passes `validate` and the contract's
    /// reference checks (media types, and the output's own references), and
    /// every named file opens, is a regular file of the command's own and
    /// fits [`Limits`]. Then every file is read aside, and only when all
    /// were read whole are they stored. Each name is read once.
    #[allow(clippy::too_many_arguments)] // each is one thing the check needs
    pub fn gather(
        &self,
        store: &dyn ArtifactStore,
        lookup: &(dyn ArtifactLookup + Sync),
        output: &mut Value,
        contract: &Value,
        validate: &dyn Fn(&Value) -> Result<(), String>,
        provenance: &Provenance,
        limits: Limits,
    ) -> Result<(), ArtifactError> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|e| refused(format!("no randomness for the stand-in references: {e}")))?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        self.gather_with(
            store, lookup, output, contract, validate, provenance, limits, &nonce,
        )
    }

    /// [`gather`](Self::gather), with the stand-ins made from `nonce`.
    #[allow(clippy::too_many_arguments)]
    fn gather_with(
        &self,
        store: &dyn ArtifactStore,
        lookup: &(dyn ArtifactLookup + Sync),
        output: &mut Value,
        contract: &Value,
        validate: &dyn Fn(&Value) -> Result<(), String>,
        provenance: &Provenance,
        limits: Limits,
        nonce: &str,
    ) -> Result<(), ArtifactError> {
        let mut candidates = Vec::new();
        named_files(output, String::new(), &mut candidates, MAX_FILE_OBJECTS + 1);
        if candidates.is_empty() {
            return Ok(());
        }
        if candidates.len() > MAX_FILE_OBJECTS {
            return Err(refused(format!(
                "the output names more than {MAX_FILE_OBJECTS} files"
            )));
        }
        // Each candidate stands in as a reference of its own, made from a
        // nonce no output can know; the contract check says which stand
        // where a reference does.
        let stand_ins: HashMap<String, usize> = (0..candidates.len())
            .map(|i| (stand_in(nonce, i).digest().to_owned(), i))
            .collect();
        if holds_any(output, &stand_ins) {
            return Err(refused(
                "the output holds a reference atomic uses while it checks one".into(),
            ));
        }
        let mut probe = output.clone();
        for (i, (pointer, _)) in candidates.iter().enumerate() {
            if let Some(slot) = probe.pointer_mut(pointer) {
                *slot = Value::String(stand_in(nonce, i).to_string());
            }
        }
        let lookup = StandIns {
            stand_ins: &stand_ins,
            candidates: &candidates,
            inner: lookup,
            asked: std::sync::Mutex::new(HashSet::new()),
        };
        let checked = atomic_schema::artifact::check_refs(contract, &probe, &lookup);
        let mut asked: Vec<usize> = lookup
            .asked
            .into_inner()
            .unwrap_or_default()
            .into_iter()
            .collect();
        asked.sort_unstable();
        // Put back the objects that are the output's own.
        let taken: HashSet<usize> = asked.iter().copied().collect();
        for (i, (pointer, original)) in candidates.iter().enumerate() {
            if !taken.contains(&i)
                && let Some(slot) = probe.pointer_mut(pointer)
            {
                *slot = original.clone();
            }
        }
        if asked.is_empty() {
            return Ok(());
        }
        // Every object well formed. A name given under two media types is
        // stored once, with a manifest for each.
        let mut files: Vec<(usize, (&str, &str))> = Vec::new();
        for &i in &asked {
            let (pointer, object) = &candidates[i];
            let named = file_object(object)
                .map_err(|why| refused(format!("{}: {why}", display_pointer(pointer))))?;
            files.push((i, named));
        }
        let pairs: std::collections::BTreeSet<(&str, &str)> =
            files.iter().map(|(_, named)| *named).collect();
        // The contract's every reference check, stand-ins and the output's
        // own references alike, then the rest of its schema.
        checked.map_err(|e| refused(e.to_string()))?;
        validate(&probe).map_err(refused)?;
        // Every file opened and held to the limits, before any is read; a
        // name counts toward the step's total once.
        let mut opened: Vec<((&str, &str), std::fs::File, u64)> = Vec::new();
        let mut sizes: BTreeMap<&str, u64> = BTreeMap::new();
        for &(name, media) in &pairs {
            let (file, size) = self.open_file(name, limits)?;
            sizes.insert(name, size);
            let total = sizes.values().fold(0u64, |t, s| t.saturating_add(*s));
            if total > limits.per_step {
                return Err(refused(format!(
                    "the files the command hands over come to more than {} bytes, the most one \
                     step may",
                    limits.per_step
                )));
            }
            opened.push(((name, media), file, size));
        }
        // Each read aside whole, up to the size it had when opened; only
        // when all are, are they stored.
        let mut staged = Vec::new();
        for (named, file, size) in opened {
            let mut exact = Exact {
                inner: file.take(size),
                left: size,
                short: false,
            };
            match store.stage(&mut exact) {
                Ok(s) => staged.push((named, s)),
                Err(e) => {
                    staged.into_iter().for_each(|(_, s)| store.discard(s));
                    return Err(if exact.short {
                        refused(format!(
                            "`{FILE_KEY}` names {:?}, which shrank while it was read",
                            named.0
                        ))
                    } else {
                        e
                    });
                }
            }
        }
        let mut stored: BTreeMap<(&str, &str), String> = BTreeMap::new();
        for (named, s) in staged {
            let manifest = store.commit(
                s,
                Provenance {
                    media_type: named.1.to_owned(),
                    ..provenance.clone()
                },
            )?;
            stored.insert(named, manifest.artifact.to_string());
        }
        for (i, named) in files {
            if let Some(slot) = output.pointer_mut(&candidates[i].0) {
                *slot = Value::String(stored[&named].clone());
            }
        }
        Ok(())
    }

    /// Open the file `name` in the outputs directory, beneath the handle
    /// held since before the command ran, and check it: a regular file with
    /// one link, within the per-file limit. Returns it and its size.
    fn open_file(&self, name: &str, limits: Limits) -> Result<(std::fs::File, u64), ArtifactError> {
        let path = self.outputs.join(name);
        let file = open_beneath(&self.outputs_dir, name).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => refused(format!(
                "`{FILE_KEY}` names {name:?}, a link: a command hands over regular files only"
            )),
            Some(libc::ENOENT) => refused(format!(
                "`{FILE_KEY}` names {name:?}, which is not in the outputs directory"
            )),
            Some(libc::EACCES) => refused(format!(
                "`{FILE_KEY}` names {name:?}, which atomic may not read"
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
        Ok((file, size))
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
/// each one's object gives, and records which the check asked about; other
/// references it asks `inner` about.
struct StandIns<'a> {
    stand_ins: &'a HashMap<String, usize>,
    candidates: &'a [(String, Value)],
    inner: &'a (dyn ArtifactLookup + Sync),
    asked: std::sync::Mutex<HashSet<usize>>,
}

impl ArtifactLookup for StandIns<'_> {
    fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>> {
        let Some(&i) = self.stand_ins.get(artifact.digest()) else {
            return self.inner.media_types(artifact);
        };
        if let Ok(mut asked) = self.asked.lock() {
            asked.insert(i);
        }
        let media = file_object(&self.candidates[i].1)
            .map(|(_, media)| media.to_owned())
            .unwrap_or_else(|_| DEFAULT_MEDIA_TYPE.to_owned());
        Some(vec![media])
    }
}

/// Whether `value` holds an object naming a file, `{"$file": …}`.
pub fn names_a_file(value: &Value) -> bool {
    let mut found = Vec::new();
    named_files(value, String::new(), &mut found, 1);
    !found.is_empty()
}

/// The most `$file` objects one output may hold.
pub const MAX_FILE_OBJECTS: usize = 10_000;

/// The reference that stands in for candidate `i` while the contract is
/// checked: a digest no content has, made from this check's `nonce`.
fn stand_in(nonce: &str, i: usize) -> ArtifactRef {
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("atomic stand-in {nonce} {i}"))
    );
    ArtifactRef::from_digest(&digest).expect("a SHA-256 digest is a reference")
}

/// Whether a string in `value` is one of the stand-in references.
fn holds_any(value: &Value, stand_ins: &HashMap<String, usize>) -> bool {
    match value {
        Value::String(s) => s
            .strip_prefix("artifact://sha256/")
            .is_some_and(|digest| stand_ins.contains_key(digest)),
        Value::Array(items) => items.iter().any(|v| holds_any(v, stand_ins)),
        Value::Object(fields) => fields.values().any(|v| holds_any(v, stand_ins)),
        _ => false,
    }
}

/// Every object in `value` with a `$file` key, with its JSON Pointer, up to
/// `most` of them.
fn named_files(value: &Value, pointer: String, out: &mut Vec<(String, Value)>, most: usize) {
    if out.len() >= most {
        return;
    }
    match value {
        Value::Object(fields) if fields.contains_key(FILE_KEY) => {
            out.push((pointer, value.clone()));
        }
        Value::Object(fields) => {
            for (key, field) in fields {
                named_files(field, format!("{pointer}/{}", escape(key)), out, most);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                named_files(item, format!("{pointer}/{i}"), out, most);
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

/// A reader of exactly `left` more bytes: one that ends sooner fails, and
/// says so in `short`, so what it read is never stored.
struct Exact<R> {
    inner: R,
    left: u64,
    short: bool,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.left > 0 && !buf.is_empty() {
            self.short = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file shrank while it was read",
            ));
        }
        self.left = self.left.saturating_sub(n as u64);
        Ok(n)
    }
}
