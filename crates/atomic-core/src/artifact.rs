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
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ArtifactError + '_ {
    move |source| ArtifactError::Io {
        path: path.to_owned(),
        source,
    }
}

impl LocalStore {
    /// A store rooted at `root`, creating its directories.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        for d in ["blobs/sha256", "manifests/sha256", "staging"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(io(&p))?;
        }
        Ok(Self { root })
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
}

/// A lock on the store's `lock` file, held until dropped: shared by writers,
/// exclusive for a sweep, so no write lands between a sweep's look at a blob
/// and its removal, and sweeps run one at a time.
struct StoreLock(#[allow(dead_code)] std::fs::File);

impl StoreLock {
    fn take(root: &Path, exclusive: bool) -> Result<Self, ArtifactError> {
        let path = root.join("lock");
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io(&path))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let how = if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            };
            // SAFETY: flock on a descriptor this function owns.
            #[allow(unsafe_code)]
            let locked = unsafe { libc::flock(file.as_raw_fd(), how) };
            if locked != 0 {
                return Err(io(&path)(std::io::Error::last_os_error()));
            }
        }
        #[cfg(not(unix))]
        let _ = exclusive;
        Ok(Self(file))
    }
}

impl LocalStore {
    /// Remove every blob, and its manifests, whose digest is not in `keep`
    /// and that was last written longer than `grace` ago. Digests in `keep`
    /// are lowercase hex, with or without a `sha256:` prefix. Partial writes
    /// left in staging longer than `grace` go too.
    ///
    /// It holds the store's lock while it runs, so a write waits for it and
    /// another sweep runs after it; content a write kept or added is then as
    /// recent as that write. What it keeps is the caller's to say: a sweep
    /// keeps nothing another caller needs unless that is in `keep` or recent.
    pub fn sweep(
        &self,
        keep: &BTreeSet<String>,
        grace: std::time::Duration,
    ) -> Result<Swept, ArtifactError> {
        let _lock = StoreLock::take(&self.root, true)?;
        let keep: BTreeSet<&str> = keep
            .iter()
            .map(|d| d.strip_prefix("sha256:").unwrap_or(d))
            .collect();
        let now = std::time::SystemTime::now();
        let old = |path: &Path| -> Result<bool, ArtifactError> {
            let written = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map_err(io(path))?;
            Ok(now.duration_since(written).is_ok_and(|age| age > grace))
        };
        let mut swept = Swept::default();
        let blobs = self.root.join("blobs/sha256");
        for prefix in std::fs::read_dir(&blobs).map_err(io(&blobs))?.flatten() {
            let prefix_path = prefix.path();
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            for blob in std::fs::read_dir(&prefix_path)
                .map_err(io(&prefix_path))?
                .flatten()
            {
                let digest = format!("{prefix_name}{}", blob.file_name().to_string_lossy());
                let Ok(artifact) = ArtifactRef::from_digest(&digest) else {
                    continue;
                };
                if keep.contains(artifact.digest()) || !old(&blob.path())? {
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
            }
        }
        swept.removed.sort_by(|a, b| a.digest().cmp(b.digest()));
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
        let _lock = StoreLock::take(&self.root, false)?;
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
        if blob.exists() {
            // Already stored: identical content is kept once, and is now as
            // recent as this write, so a sweep's grace period covers it.
            let _ = std::fs::remove_file(&tmp);
            std::fs::File::options()
                .write(true)
                .open(&blob)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()))
                .map_err(io(&blob))?;
        } else {
            let parent = blob.parent().map(Path::to_owned).unwrap_or_default();
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
