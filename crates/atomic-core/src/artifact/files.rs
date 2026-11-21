// SPDX-License-Identifier: Apache-2.0

//! Files an exec atom's command hands over, and the artifacts it is given,
//! so a large result goes between steps by reference.
//!
//! Both live in the command's workspace, inside its scratch directory, the
//! one directory a confined command may write, and atomic reaches them only
//! through the exchange the workspace's backend gives
//! ([`atomic_exec::exchange`]), so it does no filesystem work in a
//! workspace itself ([`Exchange`]):
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
//! A file the backend hands back must be a regular file of the command's
//! own, never one a link names (on this machine, see
//! [`atomic_exec::exchange::LocalExchange`]); it is read up to the size it
//! had when it was opened, and is held to [`Limits`]. An output may name at most [`MAX_FILES`] files, with
//! at most [`MAX_FILE_OBJECTS`] objects where its contract expects
//! references.

use super::{ArtifactError, ArtifactRef, ArtifactStore, Hold, Provenance};
use atomic_exec::exchange::TakeError;
use atomic_schema::artifact::ArtifactLookup;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::Path;

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

/// A command's outputs and inputs directories, as the exchange its
/// workspace's backend gave reaches them.
pub struct Exchange(Box<dyn atomic_exec::exchange::Exchange>);

impl std::fmt::Debug for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exchange")
            .field("outputs", &self.outputs())
            .field("inputs", &self.inputs())
            .finish()
    }
}

impl Exchange {
    /// The exchange a workspace's backend gave, made before the command
    /// runs ([`atomic_exec::ExecutionBackend::exchange`]).
    pub fn new(backend: Box<dyn atomic_exec::exchange::Exchange>) -> Self {
        Self(backend)
    }

    /// Where the command writes the files it hands over, as its runner sees
    /// it.
    pub fn outputs(&self) -> &Path {
        self.0.outputs()
    }

    /// Where the command reads the artifacts it is given, as its runner sees
    /// it.
    pub fn inputs(&self) -> &Path {
        self.0.inputs()
    }

    /// Give the command a copy of each of `artifacts` in its inputs
    /// directory, named by its digest and read-only, then make the
    /// directory read-only. Each copy is checked against its digest as it is
    /// made.
    pub fn mount(
        &self,
        store: &dyn ArtifactStore,
        artifacts: &[ArtifactRef],
    ) -> Result<(), ArtifactError> {
        let mut given = HashSet::new();
        for artifact in artifacts {
            if !given.insert(artifact.digest()) {
                continue;
            }
            // The store's reader says when the blob no longer matches its
            // digest; whatever the exchange itself says is its own failure.
            let mut content = Watched {
                inner: store.open(artifact)?,
                failed: false,
            };
            let given = self.0.give(artifact.digest(), &mut content);
            given.map_err(|e| {
                if content.failed && e.kind() == std::io::ErrorKind::InvalidData {
                    ArtifactError::Corrupt(artifact.clone())
                } else {
                    io(&self.inputs().join(artifact.digest()))(e)
                }
            })?;
        }
        self.0.seal().map_err(io(self.inputs()))
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
    /// fits [`Limits`]. Then every file is read aside, one at a time, and
    /// only when all were read whole are they stored. Each name is read
    /// once, and stored with a manifest for each media type it is given.
    ///
    /// Each stored file is [held](ArtifactStore::hold) before it is
    /// committed, and the holds are returned: kept until the step that
    /// gathered them has recorded its output, they keep a sweep from
    /// removing what the output references before anything records it.
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
    ) -> Result<Vec<Hold>, ArtifactError> {
        let nonce = nonce()?;
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
    ) -> Result<Vec<Hold>, ArtifactError> {
        let Some(Slots {
            candidates,
            asked,
            probe,
            checked,
        }) = slots(output, contract, lookup, nonce)?
        else {
            return Ok(Vec::new());
        };
        if asked.is_empty() {
            return Ok(Vec::new());
        }
        // Only the objects that stand where references do count.
        if asked.len() > MAX_FILE_OBJECTS {
            return Err(refused(format!(
                "the output names files with more than {MAX_FILE_OBJECTS} objects"
            )));
        }
        // Every object well formed.
        let mut files: Vec<(usize, (&str, &str))> = Vec::new();
        for &i in &asked {
            let (pointer, object) = &candidates[i];
            let named = file_object(object)
                .map_err(|why| refused(format!("{}: {why}", display_pointer(pointer))))?;
            files.push((i, named));
        }
        // A name given under several media types is read once, with a
        // manifest for each.
        let mut names: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (_, (name, media)) in &files {
            names.entry(name).or_default().insert(media);
        }
        if names.len() > MAX_FILES {
            return Err(refused(format!(
                "the output names {} files: a command hands over {MAX_FILES} at most",
                names.len()
            )));
        }
        // The contract's every reference check, stand-ins and the output's
        // own references alike, then the rest of its schema.
        checked.map_err(|e| refused(e.to_string()))?;
        validate(&probe).map_err(refused)?;
        // Every file held to the limits before any is read: each opened,
        // checked and closed in turn, a name counting toward the step's
        // total once.
        let within_step = |total: &mut u64, size: u64| {
            *total = total.saturating_add(size);
            if *total > limits.per_step {
                Err(refused(format!(
                    "the files the command hands over come to more than {} bytes, the most one \
                     step may",
                    limits.per_step
                )))
            } else {
                Ok(())
            }
        };
        let mut total = 0u64;
        for name in names.keys() {
            let (_, size) = self.take(name, limits)?;
            within_step(&mut total, size)?;
        }
        // Then each, opened and checked again, read aside whole up to the
        // size it has then, one at a time; only when all are, are they
        // stored. One staged and not stored is removed when it is dropped.
        let mut staged = Vec::with_capacity(names.len());
        let mut total = 0u64;
        for (&name, media) in &names {
            let (file, size) = self.take(name, limits)?;
            within_step(&mut total, size)?;
            let mut exact = Exact {
                inner: file.take(size),
                left: size,
                short: false,
            };
            match store.stage(&mut exact) {
                Ok(s) => staged.push((name, media, s)),
                Err(_) if exact.short => {
                    return Err(refused(format!(
                        "`{FILE_KEY}` names {name:?}, which shrank while it was read"
                    )));
                }
                Err(e) => return Err(e),
            }
        }
        let mut stored: HashMap<&str, String> = HashMap::new();
        let mut held = Vec::with_capacity(staged.len());
        for (name, media, s) in staged {
            let each: Vec<Provenance> = media
                .iter()
                .map(|media| Provenance {
                    media_type: (*media).to_owned(),
                    ..provenance.clone()
                })
                .collect();
            // Held before it is stored, so no sweep finds it stored and
            // not held, however long the rest of the step takes.
            held.push(store.hold(&s.artifact()?)?);
            let manifests = store.commit(s, &each)?;
            if let Some(manifest) = manifests.first() {
                stored.insert(name, manifest.artifact.to_string());
            }
        }
        for (i, (name, _)) in files {
            if let (Some(slot), Some(reference)) =
                (output.pointer_mut(&candidates[i].0), stored.get(name))
            {
                *slot = Value::String(reference.clone());
            }
        }
        Ok(held)
    }

    /// Take the file `name` from the outputs directory, through the
    /// backend, which checks that it is a regular file of the command's own,
    /// and hold it to the per-file limit. Returns it and its size.
    fn take(
        &self,
        name: &str,
        limits: Limits,
    ) -> Result<(Box<dyn Read + Send>, u64), ArtifactError> {
        let taken = self.0.take(name).map_err(|e| match e {
            TakeError::Link => refused(format!(
                "`{FILE_KEY}` names {name:?}, a link: a command hands over regular files only"
            )),
            TakeError::Missing => refused(format!(
                "`{FILE_KEY}` names {name:?}, which is not in the outputs directory"
            )),
            TakeError::Unreadable => refused(format!(
                "`{FILE_KEY}` names {name:?}, which atomic may not read"
            )),
            TakeError::NotRegular => refused(format!(
                "`{FILE_KEY}` names {name:?}, which is not a regular file"
            )),
            TakeError::Links(n) => refused(format!(
                "`{FILE_KEY}` names {name:?}, which has {n} links: a command hands over files \
                 of its own only"
            )),
            TakeError::Name => refused(format!(
                "`{FILE_KEY}` names {name:?}: one file name in the outputs directory, with no `/`"
            )),
            TakeError::Io(e) => io(&self.outputs().join(name))(e),
        })?;
        let (file, size) = (taken.content, taken.size);
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

/// Sixteen random bytes, in hex, for the stand-in references.
fn nonce() -> Result<String, ArtifactError> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|e| refused(format!("no randomness for the stand-in references: {e}")))?;
    Ok(nonce.iter().map(|b| format!("{b:02x}")).collect())
}

/// Where an output's `{"$file": …}` objects stand, as the contract check
/// sees them.
struct Slots {
    /// Every such object, with its JSON Pointer.
    candidates: Vec<(String, Value)>,
    /// The candidates, by index and in order, that stand where the contract
    /// expects an artifact reference.
    asked: Vec<usize>,
    /// The output with a stand-in reference in each of those places, and
    /// the output's own objects everywhere else.
    probe: Value,
    /// The contract's reference checks on `probe`.
    checked: Result<(), atomic_schema::artifact::ArtifactCheckError>,
}

/// Where `output`'s `$file` objects stand: each stands in as a reference of
/// its own, made from `nonce`, which no output can know, and the contract
/// check says which stand where a reference does. `None` when it has none.
fn slots(
    output: &Value,
    contract: &Value,
    lookup: &(dyn ArtifactLookup + Sync),
    nonce: &str,
) -> Result<Option<Slots>, ArtifactError> {
    let mut candidates = Vec::new();
    named_files(output, String::new(), &mut candidates, usize::MAX);
    if candidates.is_empty() {
        return Ok(None);
    }
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
    Ok(Some(Slots {
        candidates,
        asked,
        probe,
        checked,
    }))
}

/// Whether `output` names a file where `contract` expects an artifact
/// reference: what [`Exchange::gather`] would take. A `{"$file": …}` object
/// anywhere else is the output's own.
pub fn names_a_file_where_referenced(
    output: &Value,
    contract: &Value,
    lookup: &(dyn ArtifactLookup + Sync),
) -> Result<bool, ArtifactError> {
    Ok(slots(output, contract, lookup, &nonce()?)?.is_some_and(|s| !s.asked.is_empty()))
}

/// The most `$file` objects one output may hold where its contract expects
/// artifact references. Objects anywhere else are the output's own.
pub const MAX_FILE_OBJECTS: usize = 10_000;

/// The most distinct files one output may name.
pub const MAX_FILES: usize = 256;

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

/// A reader that notes whether it failed, so its failure is told apart
/// from that of whatever reads it.
struct Watched<R> {
    inner: R,
    failed: bool,
}

impl<R: Read> Read for Watched<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).inspect_err(|_| self.failed = true)
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
