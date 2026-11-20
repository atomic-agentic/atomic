// SPDX-License-Identifier: Apache-2.0

//! The content-addressed artifact store.
//!
//! Large results (patches, reports, logs, generated files) are stored once by
//! the SHA-256 of their content and passed around as [`ArtifactRef`]s. Each
//! producer also records a small [`Manifest`]: media type, size, the workflow
//! and step that produced it, its origin and sensitivity labels. One blob can
//! have several manifests when different steps produce the same bytes.
//!
//! [`LocalStore`] keeps everything in a directory, by default
//! `<state>/artifacts/`:
//!
//! ```text
//! blobs/sha256/<2 hex>/<62 hex>          the content, written once
//! manifests/sha256/<digest>/<hash>.json  one file per distinct manifest
//! staging/<owner>-<random>               partial writes, renamed into place
//! owners/<owner>                         locked by each open store
//! ```
//!
//! Writes are atomic: content is streamed to a staging file while it is
//! hashed, synced, made read-only, then renamed into place, so a crash never
//! leaves a blob under its digest that does not match it. Steps run at least
//! once, and writing the same content again stores nothing new, unless the
//! blob under its digest no longer matches it, which the new content then
//! replaces. Reading a blob checks its digest as it goes: a reader that
//! reaches the end of a blob that does not match fails.
//!
//! Each open store holds a lock on a file of its own under `owners/`, and
//! names its staging files after it. A staging file outlives its write only
//! when the process writing it died; opening the store removes such files
//! once no one holds their owner's lock and they are [`STAGING_GRACE`] old.

pub mod files;

pub use atomic_schema::artifact::ArtifactRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What a producer records about an artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The artifact this describes.
    pub artifact: ArtifactRef,
    /// Media type, for example `text/x-diff`.
    pub media_type: String,
    /// Size in bytes.
    pub size: u64,
    /// The workflow run that produced it, if any.
    pub workflow: Option<String>,
    /// The step that produced it, if any.
    pub step: Option<String>,
    /// Where the content came from, for example a tool or an upload.
    pub origin: Option<String>,
    /// Sensitivity labels, for example `pii` or `secret`.
    pub labels: BTreeSet<String>,
}

/// What a producer says about content it stores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Media type, for example `text/x-diff`.
    pub media_type: String,
    /// The producing workflow run.
    pub workflow: Option<String>,
    /// The producing step.
    pub step: Option<String>,
    /// Where the content came from.
    pub origin: Option<String>,
    /// Sensitivity labels.
    pub labels: BTreeSet<String>,
}

/// A failure of the artifact store.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// The artifact is not in the store.
    #[error("no artifact {0}")]
    Missing(ArtifactRef),
    /// The filesystem failed.
    #[error("artifact store at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A command's output names a file atomic will not take, or the file
    /// breaks a limit.
    #[error("{0}")]
    Refused(String),
    /// A stored blob no longer matches its digest.
    #[error("artifact {0} does not match its digest: the store is damaged")]
    Corrupt(ArtifactRef),
    /// A manifest could not be read or written.
    #[error("artifact manifest {path}: {source}")]
    Manifest {
        /// The manifest file.
        path: PathBuf,
        /// The underlying error.
        source: serde_json::Error,
    },
}

/// Where artifacts live. The local directory implements it now; object stores
/// follow behind the same trait.
pub trait ArtifactStore {
    /// Store the content read from `content` and record `provenance` for it.
    /// Identical content is stored once.
    fn put(
        &self,
        content: &mut dyn Read,
        provenance: Provenance,
    ) -> Result<Manifest, ArtifactError> {
        let staged = self.stage(content)?;
        let mut manifests = self.commit(staged, std::slice::from_ref(&provenance))?;
        Ok(manifests.remove(0))
    }

    /// Read `content` aside, hashed but not yet stored: [`commit`] stores
    /// it, [`discard`] drops it, and so does dropping it. Several contents
    /// can be staged and then all committed, so that none is stored when one
    /// fails.
    ///
    /// [`commit`]: Self::commit
    /// [`discard`]: Self::discard
    fn stage(&self, content: &mut dyn Read) -> Result<Staged, ArtifactError>;

    /// Store what [`stage`](Self::stage) read and record a manifest for each
    /// of `provenance`, returned in its order.
    fn commit(
        &self,
        staged: Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<Manifest>, ArtifactError>;

    /// Drop what [`stage`](Self::stage) read, storing nothing.
    fn discard(&self, staged: Staged);

    /// Whether the content is present.
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError>;

    /// Open the content for reading.
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError>;

    /// Every manifest recorded for the content, in a stable order.
    fn manifests(&self, artifact: &ArtifactRef) -> Result<Vec<Manifest>, ArtifactError>;
}

/// Content [`ArtifactStore::stage`] read aside, not yet stored. Dropped
/// uncommitted, as when its holder fails or panics, it is removed.
#[derive(Debug)]
pub struct Staged {
    /// Where it waits; empty once it has been moved or removed.
    path: PathBuf,
    /// Its SHA-256, in hex.
    digest: String,
    /// Its size, in bytes.
    size: u64,
}

impl Staged {
    /// Its size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its path, which it no longer removes.
    fn take(&mut self) -> PathBuf {
        std::mem::take(&mut self.path)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How old a staging file must be, with no one holding its owner's lock,
/// before opening a store removes it: long enough that a store just opening
/// has taken its lock.
pub const STAGING_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// An artifact store in a local directory.
#[derive(Clone, Debug)]
pub struct LocalStore {
    root: PathBuf,
    owner: std::sync::Arc<Owner>,
}

/// An open store's claim on the staging files it names after itself: a
/// lock held on `owners/<id>` while the store is open, which a sweep finds
/// taken.
#[derive(Debug)]
struct Owner {
    id: String,
    path: PathBuf,
    _lock: std::fs::File,
}

impl Owner {
    fn take(root: &Path) -> Result<Self, ArtifactError> {
        let id = random_name()?;
        let path = root.join("owners").join(&id);
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io(&path))?;
        lock.try_lock().map_err(|e| ArtifactError::Io {
            path: path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        Ok(Self {
            id,
            path,
            _lock: lock,
        })
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sixteen random bytes, in hex: a name no other process picks.
fn random_name() -> Result<String, ArtifactError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ArtifactError::Io {
        path: PathBuf::from("getrandom"),
        source: std::io::Error::other(e.to_string()),
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Remove what dead stores left in `root`'s staging directory: each file at
/// least `grace` old whose owner no one holds, and each such owner's file.
/// A file it cannot judge is left.
fn sweep(root: &Path, grace: std::time::Duration) {
    let now = std::time::SystemTime::now();
    let old = |path: &Path| {
        std::fs::symlink_metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= grace)
    };
    // The owners someone holds, or that are too new to judge.
    let mut live = std::collections::HashSet::new();
    let mut dead = Vec::new();
    for entry in std::fs::read_dir(root.join("owners"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let (path, name) = (
            entry.path(),
            entry.file_name().to_string_lossy().into_owned(),
        );
        let held = match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => {
                    if old(&path) {
                        dead.push((path, file));
                    } else {
                        live.insert(name.clone());
                    }
                    continue;
                }
                Err(_) => true,
            },
            Err(_) => true,
        };
        if held {
            live.insert(name);
        }
    }
    for entry in std::fs::read_dir(root.join("staging"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let owner = name.split('-').next().unwrap_or_default();
        if !live.contains(owner) && old(&entry.path()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // Removed while locked, so no one judges it again meanwhile.
    for (path, _file) in dead {
        let _ = std::fs::remove_file(&path);
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ArtifactError + '_ {
    move |source| ArtifactError::Io {
        path: path.to_owned(),
        source,
    }
}

impl LocalStore {
    /// A store rooted at `root`, creating its directories.
    ///
    /// It removes what stores that died left in staging (see the module's
    /// documentation).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        Self::open_with_grace(root, STAGING_GRACE)
    }

    /// [`open`](Self::open), removing dead stores' staging files once they
    /// are `grace` old.
    pub fn open_with_grace(
        root: impl Into<PathBuf>,
        grace: std::time::Duration,
    ) -> Result<Self, ArtifactError> {
        let root = root.into();
        for d in ["blobs/sha256", "manifests/sha256", "staging", "owners"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(io(&p))?;
        }
        let owner = std::sync::Arc::new(Owner::take(&root)?);
        sweep(&root, grace);
        Ok(Self { root, owner })
    }

    /// A new staging file, created here and nowhere else, named after this
    /// store's owner.
    fn staging_file(&self) -> Result<(PathBuf, std::fs::File), ArtifactError> {
        let path = self
            .root
            .join("staging")
            .join(format!("{}-{}", self.owner.id, random_name()?));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io(&path))?;
        Ok((path, file))
    }

    fn blob_path(&self, a: &ArtifactRef) -> PathBuf {
        let d = a.digest();
        self.root.join("blobs/sha256").join(&d[..2]).join(&d[2..])
    }

    fn manifest_dir(&self, a: &ArtifactRef) -> PathBuf {
        self.root.join("manifests/sha256").join(a.digest())
    }

    /// Write `bytes` to `dest` through a staging file and a rename.
    fn place(
        &self,
        dest: &Path,
        write: impl FnOnce(&mut std::fs::File) -> Result<(), ArtifactError>,
    ) -> Result<(), ArtifactError> {
        let (tmp, mut f) = self.staging_file()?;
        let result = (|| {
            write(&mut f)?;
            f.sync_all().map_err(io(&tmp))?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(io(parent))?;
            }
            std::fs::rename(&tmp, dest).map_err(io(dest))?;
            if let Some(parent) = dest.parent() {
                sync_dir(parent)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// Make a stored file read-only: 0444 where modes exist.
fn set_read_only(path: &Path) -> Result<(), ArtifactError> {
    let mut permissions = std::fs::metadata(path).map_err(io(path))?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o444);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).map_err(io(path))
}

/// Whether the blob at `path` still holds `size` bytes whose SHA-256 is
/// `digest`. A blob of the wrong size is not read.
fn blob_matches(path: &Path, digest: &str, size: u64) -> bool {
    if std::fs::metadata(path).map(|m| m.len()).ok() != Some(size) {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).is_ok() && format!("{:x}", hasher.finalize()) == digest
}

/// A blob's reader that checks the blob's digest as it reads: at the end of
/// one that no longer matches, it fails rather than report the end. Only a
/// reader that reads to the end is checked: one that stops before it has
/// read bytes no check vouched for.
struct Verified {
    inner: std::fs::File,
    hasher: Sha256,
    artifact: ArtifactRef,
    /// Whether the end was reached and matched.
    ended: bool,
}

impl Read for Verified {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // An empty buffer reads nothing, and is not the end.
        if self.ended || buf.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
            return Ok(n);
        }
        let digest = format!("{:x}", std::mem::take(&mut self.hasher).finalize());
        if digest == self.artifact.digest() {
            self.ended = true;
            Ok(0)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                ArtifactError::Corrupt(self.artifact.clone()).to_string(),
            ))
        }
    }
}

fn sync_dir(dir: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io(dir))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

impl ArtifactStore for LocalStore {
    fn stage(&self, content: &mut dyn Read) -> Result<Staged, ArtifactError> {
        // Stream to staging while hashing; committing moves it into place
        // under its digest.
        let (tmp, mut f) = self.staging_file()?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let staged = (|| {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = content.read(&mut buf).map_err(io(&tmp))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n]).map_err(io(&tmp))?;
                size += n as u64;
            }
            f.sync_all().map_err(io(&tmp))?;
            // Read-only before it is in place: no one writes a stored blob.
            set_read_only(&tmp)
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(Staged {
            path: tmp,
            digest: format!("{:x}", hasher.finalize()),
            size,
        })
    }

    fn commit(
        &self,
        mut staged: Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<Manifest>, ArtifactError> {
        let (digest, size) = (staged.digest.clone(), staged.size);
        let artifact = ArtifactRef::from_digest(&digest).map_err(|e| ArtifactError::Io {
            path: staged.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        let blob = self.blob_path(&artifact);
        if !(blob.exists() && blob_matches(&blob, &digest, size)) {
            let parent = blob.parent().map(Path::to_owned).unwrap_or_default();
            std::fs::create_dir_all(&parent).map_err(io(&parent))?;
            std::fs::rename(&staged.path, &blob).map_err(io(&blob))?;
            staged.take();
            sync_dir(&parent)?;
        }
        // Already stored, identical content is kept once, and what was
        // staged goes when it is dropped; so does a failed move's.
        drop(staged);

        let mut manifests = Vec::with_capacity(provenance.len());
        for provenance in provenance {
            let manifest = Manifest {
                artifact: artifact.clone(),
                media_type: provenance.media_type.clone(),
                size,
                workflow: provenance.workflow.clone(),
                step: provenance.step.clone(),
                origin: provenance.origin.clone(),
                labels: provenance.labels.clone(),
            };
            let json =
                serde_json::to_vec_pretty(&manifest).map_err(|source| ArtifactError::Manifest {
                    path: self.manifest_dir(&artifact),
                    source,
                })?;
            let name = format!("{:x}.json", Sha256::digest(&json));
            let path = self.manifest_dir(&artifact).join(name);
            if !path.exists() {
                self.place(&path, |f| f.write_all(&json).map_err(io(&path)))?;
            }
            manifests.push(manifest);
        }
        Ok(manifests)
    }

    fn discard(&self, staged: Staged) {
        drop(staged);
    }

    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError> {
        let p = self.blob_path(artifact);
        match std::fs::metadata(&p) {
            Ok(m) => Ok(m.is_file()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io(&p)(e)),
        }
    }

    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError> {
        let p = self.blob_path(artifact);
        match std::fs::File::open(&p) {
            Ok(f) => Ok(Box::new(Verified {
                inner: f,
                hasher: Sha256::new(),
                artifact: artifact.clone(),
                ended: false,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ArtifactError::Missing(artifact.clone()))
            }
            Err(e) => Err(io(&p)(e)),
        }
    }

    fn manifests(&self, artifact: &ArtifactRef) -> Result<Vec<Manifest>, ArtifactError> {
        let dir = self.manifest_dir(artifact);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&dir)(e)),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(&p).map_err(io(&p))?;
                serde_json::from_slice(&bytes)
                    .map_err(|source| ArtifactError::Manifest { path: p, source })
            })
            .collect()
    }
}

impl atomic_schema::artifact::ArtifactLookup for LocalStore {
    fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>> {
        if !self.contains(artifact).ok()? {
            return None;
        }
        let mut types: Vec<String> = self
            .manifests(artifact)
            .ok()?
            .into_iter()
            .map(|m| m.media_type)
            .collect();
        types.sort();
        types.dedup();
        Some(types)
    }
}

#[cfg(test)]
mod tests;
