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
//! staging/                               partial writes, renamed into place
//! ```
//!
//! Writes are atomic: content is streamed to a staging file while it is
//! hashed, synced, then renamed into place, so a crash never leaves a blob
//! under its digest that does not match it. Steps run at least once, and
//! writing the same content again stores nothing new (it only marks the
//! content as just written, for [`LocalStore::sweep`]).
//!
//! Retention is a sweep over the store with a set of digests to keep:
//! whatever the caller says is still referenced, such as the evidence gates
//! pin (`atomic_app::retention`). Content written recently is kept too, so a
//! step that has stored a result but not yet recorded a reference to it
//! never loses it.

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
    /// The store's lock was held elsewhere for longer than this waits.
    #[error(
        "the artifact store's lock {path} was held elsewhere for {waited:?}; another atomic, or \
         something holding the file, is in the way"
    )]
    Locked {
        /// The lock file.
        path: PathBuf,
        /// How long it waited.
        waited: std::time::Duration,
    },
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
    ) -> Result<Manifest, ArtifactError>;

    /// Whether the content is present.
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError>;

    /// Open the content for reading.
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError>;

    /// Every manifest recorded for the content, in a stable order.
    fn manifests(&self, artifact: &ArtifactRef) -> Result<Vec<Manifest>, ArtifactError>;
}

/// An artifact store in a local directory.
#[derive(Clone, Debug)]
pub struct LocalStore {
    root: PathBuf,
    lock: PathBuf,
    lock_wait: std::time::Duration,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ArtifactError + '_ {
    move |source| ArtifactError::Io {
        path: path.to_owned(),
        source,
    }
}

impl LocalStore {
    /// A store rooted at `root`, creating its directories, with its lock
    /// file in `root`: for a store no atom's sandbox can read, such as a
    /// test's. A project's store is [`Self::for_project`].
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        let lock = root.join("lock");
        Self::open_locked_at(root, lock)
    }

    /// The project's store, `<state>/artifacts`, with its lock file in the
    /// state directory's `private/`, which atoms' commands cannot open: an
    /// atom that could take the lock could stall every write.
    pub fn for_project(state: &crate::state::ProjectState) -> Result<Self, ArtifactError> {
        Self::open_locked_at(
            state.dir().join("artifacts"),
            state.private_dir().join("artifacts.lock"),
        )
    }

    fn open_locked_at(root: PathBuf, lock: PathBuf) -> Result<Self, ArtifactError> {
        for d in ["blobs/sha256", "manifests/sha256", "staging"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(io(&p))?;
        }
        if let Some(dir) = lock.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        Ok(Self {
            root,
            lock,
            lock_wait: LOCK_WAIT,
        })
    }

    /// The same store, waiting at most `wait` for its lock.
    pub fn waiting_at_most(mut self, wait: std::time::Duration) -> Self {
        self.lock_wait = wait;
        self
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
        let staging = self.root.join("staging");
        let tmp = staging.join(format!(
            "{}-{}",
            std::process::id(),
            STAGING_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let result = (|| {
            let mut f = std::fs::File::create(&tmp).map_err(io(&tmp))?;
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

static STAGING_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Make `f` readable by everyone and writable by no one.
fn read_only(f: &std::fs::File) -> std::io::Result<()> {
    let mut permissions = f.metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o444);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    f.set_permissions(permissions)
}

/// Whether the blob at `path` exists and still holds the content named
/// `digest`, `size` bytes long. A blob of the wrong size, or whose bytes no
/// longer hash to its name, is not.
fn intact(path: &Path, digest: &str, size: u64) -> Result<bool, ArtifactError> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(path)(e)),
    };
    let meta = f.metadata().map_err(io(path))?;
    if !meta.is_file() || meta.len() != size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).map_err(io(path))?;
    Ok(format!("{:x}", hasher.finalize()) == digest)
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

/// What a [`LocalStore::sweep`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// The content it removed, with its manifests.
    pub removed: Vec<ArtifactRef>,
    /// How many blobs it kept.
    pub kept: usize,
    /// How many stale partial writes it cleared from staging.
    pub partial: usize,
    /// Entries in the blob tree that are not content, left as they are.
    pub skipped: Vec<PathBuf>,
}

/// How long a sweep keeps what was just written, at the least: a caller's
/// grace below it is raised to it (see [`Grace`]).
pub const MIN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// How long [`LocalStore::sweep`] keeps content written recently: at least
/// [`MIN_GRACE`], so a step that stored a result and has not yet recorded a
/// reference to it keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grace(std::time::Duration);

impl Grace {
    /// `grace`, or [`MIN_GRACE`] if that is longer.
    pub fn at_least(grace: std::time::Duration) -> Self {
        Self(grace.max(MIN_GRACE))
    }

    /// No grace at all: for tests, which date content themselves.
    #[cfg(any(test, feature = "fake-model"))]
    pub fn none_for_tests() -> Self {
        Self(std::time::Duration::ZERO)
    }
}

/// How long a write or a sweep waits for the store's lock before it gives up
/// with [`ArtifactError::Locked`].
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A lock on the store's lock file, held until dropped: shared by writers,
/// exclusive for a sweep, so no write lands between a sweep's look at a blob
/// and its removal, and sweeps run one at a time. Taken without blocking,
/// retried until a deadline, so a process holding it forever stalls nothing
/// for longer than that.
struct StoreLock(#[allow(dead_code)] std::fs::File);

impl StoreLock {
    fn take(
        path: &Path,
        exclusive: bool,
        wait: std::time::Duration,
    ) -> Result<Self, ArtifactError> {
        // Read and write: some network filesystems refuse a shared lock on a
        // descriptor that cannot read.
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(io(path))?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let tried = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match tried {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(ArtifactError::Locked {
                        path: path.to_owned(),
                        waited: wait,
                    });
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(io(path)(e)),
            }
        }
    }
}

/// Whether `name` is a two-hex-digit prefix directory of the blob tree.
fn is_prefix(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl LocalStore {
    /// Remove every blob, and its manifests, that `keep` does not name and
    /// that was last written longer than `grace` ago, and partial writes
    /// left in staging as long. Entries in the blob tree that are not
    /// content (a stray file, a prefix directory that is a link) are left
    /// alone and reported in [`Swept::skipped`].
    ///
    /// It holds the store's lock while it runs, so a write waits for it and
    /// another sweep runs after it; content a write kept or added is then as
    /// recent as that write. What it keeps is the caller's to say: a sweep
    /// keeps nothing another caller needs unless that is in `keep` or recent.
    pub fn sweep(
        &self,
        keep: &BTreeSet<ArtifactRef>,
        grace: Grace,
    ) -> Result<Swept, ArtifactError> {
        let _lock = StoreLock::take(&self.lock, true, self.lock_wait)?;
        let grace = grace.0;
        let now = std::time::SystemTime::now();
        let old = |path: &Path| -> Result<bool, ArtifactError> {
            let written = std::fs::symlink_metadata(path)
                .and_then(|m| m.modified())
                .map_err(io(path))?;
            Ok(now.duration_since(written).is_ok_and(|age| age > grace))
        };
        let mut swept = Swept::default();
        let blobs = self.root.join("blobs/sha256");
        for prefix in std::fs::read_dir(&blobs).map_err(io(&blobs))?.flatten() {
            let prefix_path = prefix.path();
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            // Never through a link, and only the tree's own two-hex prefixes.
            let is_dir = prefix.file_type().is_ok_and(|t| t.is_dir());
            if !is_dir || !is_prefix(&prefix_name) {
                swept.skipped.push(prefix_path);
                continue;
            }
            for blob in std::fs::read_dir(&prefix_path)
                .map_err(io(&prefix_path))?
                .flatten()
            {
                let digest = format!("{prefix_name}{}", blob.file_name().to_string_lossy());
                let is_file = blob.file_type().is_ok_and(|t| t.is_file());
                let artifact = match ArtifactRef::from_digest(&digest) {
                    Ok(artifact) if is_file => artifact,
                    _ => {
                        swept.skipped.push(blob.path());
                        continue;
                    }
                };
                if keep.contains(&artifact) || !old(&blob.path())? {
                    swept.kept += 1;
                    continue;
                }
                // The manifests first: a blob with no manifest is still
                // whole, a manifest with no blob would point at nothing.
                let manifests = self.manifest_dir(&artifact);
                match std::fs::remove_dir_all(&manifests) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(io(&manifests)(e));
                    }
                    _ => {}
                }
                std::fs::remove_file(blob.path()).map_err(io(&blob.path()))?;
                swept.removed.push(artifact);
            }
        }
        let staging = self.root.join("staging");
        for partial in std::fs::read_dir(&staging).map_err(io(&staging))?.flatten() {
            if old(&partial.path())? {
                let _ = std::fs::remove_file(partial.path());
                swept.partial += 1;
            }
        }
        swept.removed.sort();
        Ok(swept)
    }
}

impl ArtifactStore for LocalStore {
    fn put(
        &self,
        content: &mut dyn Read,
        provenance: Provenance,
    ) -> Result<Manifest, ArtifactError> {
        // Stream to staging while hashing, then move into place under the
        // digest, with no sweep in between.
        let _lock = StoreLock::take(&self.lock, false, self.lock_wait)?;
        let staging = self.root.join("staging");
        let tmp = staging.join(format!(
            "{}-{}",
            std::process::id(),
            STAGING_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let staged = (|| {
            let mut f = std::fs::File::create(&tmp).map_err(io(&tmp))?;
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
            // Read-only once written: content under its digest is never
            // changed in place, by atomic or through a link a command holds.
            read_only(&f).map_err(io(&tmp))?;
            f.sync_all().map_err(io(&tmp))
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let digest = format!("{:x}", hasher.finalize());
        let artifact = ArtifactRef::from_digest(&digest).map_err(|e| ArtifactError::Io {
            path: tmp.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        let blob = self.blob_path(&artifact);
        let parent = blob.parent().map(Path::to_owned).unwrap_or_default();
        if intact(&blob, &digest, size)? {
            // Already stored: identical content is kept once, and is now as
            // recent as this write, so a sweep's grace period covers it. The
            // blob is opened read-only (its owner may set its times). The
            // time is not synced: after a crash it may be older, and the
            // content is then only as safe as the caller's keep set.
            let _ = std::fs::remove_file(&tmp);
            std::fs::File::open(&blob)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()))
                .map_err(io(&blob))?;
        } else {
            // New, or a blob that no longer holds its content: the content
            // just written takes its place.
            std::fs::create_dir_all(&parent).map_err(io(&parent))?;
            std::fs::rename(&tmp, &blob).map_err(io(&blob))?;
            sync_dir(&parent)?;
        }

        let manifest = Manifest {
            artifact: artifact.clone(),
            media_type: provenance.media_type,
            size,
            workflow: provenance.workflow,
            step: provenance.step,
            origin: provenance.origin,
            labels: provenance.labels,
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
        Ok(manifest)
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
            Ok(f) => Ok(Box::new(f)),
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
