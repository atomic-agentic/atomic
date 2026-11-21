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
//! ```
//!
//! Writes are atomic: content is streamed to a staging file while it is
//! hashed, made read-only, synced, then renamed into place, so a crash never
//! leaves a blob under its digest that does not match it, and no one changes
//! a blob in place through a link to it. Steps run at least once, and writing
//! the same content again stores nothing new: it marks the content as just
//! written, for [`LocalStore::sweep`], and replaces a blob that no longer
//! holds its content.
//!
//! A project's store takes its lock in the project's private directory,
//! which atoms' commands cannot open; writes share it, and a sweep holds it
//! alone. Waiting for it is bounded: past [`LOCK_WAIT`], a write or a sweep
//! fails with [`ArtifactError::Locked`], which is transient.
//!
//! Retention is a sweep over the store with a set of digests to keep, read
//! at a given time: whatever the caller says is still referenced (for runs
//! and gates, `atomic_app::retention` finds it). Content written within the
//! grace period before that time is kept too, so a step that has stored a
//! result but not yet recorded a reference to it never loses it.
//!
//! Each open store holds a lock on a file of its own, its owner, in an
//! `artifact-owners` directory beside the store's lock, and names its
//! staging files after it. A staging file outlives its write only when the
//! process writing it died: opening the store removes such files once no
//! one holds their owner's lock and they are [`STAGING_GRACE`] old, and a
//! sweep leaves alone every staging file whose owner is held.
//!
//! A store can also [`hold`](ArtifactStore::hold) content: a sweep keeps
//! it, however old, while the [`Hold`] is kept and the store that took it
//! is open. A hold is taken under the store's shared lock, as a write is, so
//! it waits for a sweep in progress and can fail with
//! [`ArtifactError::Locked`]. A gather holds each file it stores until its step has recorded
//! the reference, so a sweep between the two never removes it, however long
//! the step takes to get there. A hold is a file in `artifact-held/<owner>/`
//! beside the owners; a dead store's holds go with its staging files.

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

    /// Keep `artifact` from every sweep, however old it is and whether or
    /// not it is stored yet, until the [`Hold`] is dropped or this store is
    /// no longer open. Once it returns, every sweep either had finished
    /// before it, so content that sweep removed is gone, or keeps the
    /// content while the hold is kept.
    fn hold(&self, artifact: &ArtifactRef) -> Result<Hold, ArtifactError>;

    /// Whether the content is present.
    fn contains(&self, artifact: &ArtifactRef) -> Result<bool, ArtifactError>;

    /// Open the content for reading.
    fn open(&self, artifact: &ArtifactRef) -> Result<Box<dyn Read + Send>, ArtifactError>;

    /// Every manifest recorded for the content, in a stable order.
    fn manifests(&self, artifact: &ArtifactRef) -> Result<Vec<Manifest>, ArtifactError>;

    /// How long a write, a hold or a sweep waits for the store's lock before
    /// it fails with [`ArtifactError::Locked`].
    fn lock_wait(&self) -> std::time::Duration {
        LOCK_WAIT
    }
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

    /// The reference it is stored under once committed.
    pub fn artifact(&self) -> Result<ArtifactRef, ArtifactError> {
        ArtifactRef::from_digest(&self.digest).map_err(|e| ArtifactError::Io {
            path: self.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })
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

/// A store's word that every sweep keeps an artifact while this is kept
/// and the store that gave it is open (see [`ArtifactStore::hold`]).
/// Dropped, it lets go.
#[derive(Debug)]
pub struct Hold {
    /// The file that records it, removed when it is dropped.
    path: PathBuf,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
    lock: PathBuf,
    lock_wait: std::time::Duration,
    owner: std::sync::Arc<Owner>,
}
/// An open store's claim on the staging files it names after itself: a
/// lock held on its owner file while the store is open, which a sweep finds
/// taken.
#[derive(Debug)]
struct Owner {
    id: String,
    path: PathBuf,
    /// Where its holds are, `artifact-held/<id>`.
    held: PathBuf,
    _lock: std::fs::File,
}

impl Owner {
    /// A new owner in `owners`, locked, with its holds in `held`. Only a
    /// sweep passing by can hold a new owner's lock, and only while it
    /// looks, so this waits for it.
    fn take(owners: &Path, held: &Path) -> Result<Self, ArtifactError> {
        let id = random_name()?;
        let path = owners.join(&id);
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io(&path))?;
        lock.lock().map_err(io(&path))?;
        Ok(Self {
            held: held.join(&id),
            id,
            path,
            _lock: lock,
        })
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Its holds first: holds with no owner file are kept a while.
        let _ = std::fs::remove_dir_all(&self.held);
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

/// Whether `path` was last written at least `grace` before `now`. One it
/// cannot judge is not.
fn older_than(path: &Path, grace: std::time::Duration, now: std::time::SystemTime) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age >= grace)
}

/// The owners in `owners` whose staging files no one may remove: each one
/// someone holds, and each younger than `grace`, whose store may still be
/// taking its lock and is not tried; with the files of the dead ones,
/// locked, so they can be removed while no one else judges them.
fn live_owners(
    owners: &Path,
    grace: std::time::Duration,
    now: std::time::SystemTime,
) -> (
    std::collections::HashSet<String>,
    Vec<(PathBuf, std::fs::File)>,
) {
    let mut live = std::collections::HashSet::new();
    let mut dead = Vec::new();
    for entry in std::fs::read_dir(owners).into_iter().flatten().flatten() {
        let (path, name) = (
            entry.path(),
            entry.file_name().to_string_lossy().into_owned(),
        );
        if older_than(&path, grace, now)
            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path)
            && file.try_lock().is_ok()
        {
            dead.push((path, file));
        } else {
            live.insert(name);
        }
    }
    (live, dead)
}

/// The owner a staging file `name` was written by.
fn staged_by(name: &str) -> &str {
    name.split('-').next().unwrap_or_default()
}

/// The holds in `held` a sweep keeps to, by digest, and the owners' hold
/// directories it may remove. Every owner's holds are kept to but those of
/// the `dead`, which the caller has locked, and those whose owner file is
/// gone and that are `grace` old: a store just opening may write its holds
/// after the caller looked at the owners. A directory it cannot read fails
/// it, rather than leave what it holds out of what a sweep keeps.
fn holds(
    held: &Path,
    owners: &Path,
    dead: &[(PathBuf, std::fs::File)],
    grace: std::time::Duration,
    now: std::time::SystemTime,
) -> Result<(std::collections::HashSet<String>, Vec<PathBuf>), ArtifactError> {
    let dead: std::collections::HashSet<_> = dead
        .iter()
        .filter_map(|(path, _)| path.file_name())
        .collect();
    let mut kept = std::collections::HashSet::new();
    let mut gone = Vec::new();
    let entries = match std::fs::read_dir(held) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((kept, gone)),
        Err(e) => return Err(io(held)(e)),
    };
    for entry in entries {
        let entry = entry.map_err(io(held))?;
        let (path, name) = (entry.path(), entry.file_name());
        let orphan = || {
            std::fs::symlink_metadata(owners.join(&name))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && older_than(&path, grace, now)
        };
        if dead.contains(name.as_os_str()) || orphan() {
            gone.push(path);
            continue;
        }
        if !entry.file_type().map_err(io(&path))?.is_dir() {
            continue;
        }
        // A store closing meanwhile takes its holds with it.
        let each = match std::fs::read_dir(&path) {
            Ok(each) => each,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io(&path)(e)),
        };
        for hold in each {
            let hold = match hold {
                Ok(hold) => hold,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io(&path)(e)),
            };
            kept.insert(staged_by(&hold.file_name().to_string_lossy()).to_owned());
        }
    }
    Ok((kept, gone))
}

/// Remove what dead stores left in `staging`: each file at least `grace`
/// old whose owner, in `owners`, no one holds, each such owner's holds in
/// `held`, and its file.
fn sweep_staging(owners: &Path, held: &Path, staging: &Path, grace: std::time::Duration) {
    let now = std::time::SystemTime::now();
    let (live, dead) = live_owners(owners, grace, now);
    for entry in std::fs::read_dir(staging).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !live.contains(staged_by(&name)) && older_than(&entry.path(), grace, now) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    if let Ok((_, gone)) = holds(held, owners, &dead, grace, now) {
        for path in gone {
            let _ = std::fs::remove_dir_all(path);
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
    /// A store rooted at `root`, creating its directories. A project's store
    /// in an atomic home, `<home>/projects/<id>/artifacts`, takes the same
    /// lock as [`Self::for_project`], `<home>/private/<id>/artifacts.lock`:
    /// `<home>` is an atomic home when it is the configured one or already has
    /// its `private/`. The project's private directory is made, readable by
    /// its owner only, when it is not there; one that is there and readable
    /// by others is refused, never changed. Any other store, outside every
    /// atomic home (such as a test's), keeps its lock file in `root`.
    ///
    /// It removes what stores that died left in staging (see the module's
    /// documentation).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        Self::open_sweeping_after(root.into(), STAGING_GRACE)
    }

    /// [`open`](Self::open), removing dead stores' staging files once they
    /// are `grace` old.
    #[cfg(test)]
    pub(crate) fn open_with_grace(
        root: impl Into<PathBuf>,
        grace: std::time::Duration,
    ) -> Result<Self, ArtifactError> {
        Self::open_sweeping_after(root.into(), grace)
    }

    fn open_sweeping_after(
        root: PathBuf,
        grace: std::time::Duration,
    ) -> Result<Self, ArtifactError> {
        let lock = match project_private(&root) {
            Some(private) => {
                own_private_dir(&private)?;
                private.join("artifacts.lock")
            }
            None => root.join("lock"),
        };
        Self::open_locked_at(root, lock, grace)
    }

    /// Every blob the store holds, by its digest. Takes no lock: content
    /// written while it reads may or may not be listed. An entry it cannot
    /// read fails it, rather than leaving that content out of what a sweep
    /// looks for; one that is not content (a stray file, a link) is passed
    /// over, as a sweep passes it over.
    pub fn stored(&self) -> Result<BTreeSet<ArtifactRef>, ArtifactError> {
        let mut all = BTreeSet::new();
        let blobs = self.root.join("blobs/sha256");
        for prefix in std::fs::read_dir(&blobs).map_err(io(&blobs))? {
            let prefix = prefix.map_err(io(&blobs))?;
            let path = prefix.path();
            let name = prefix.file_name().to_string_lossy().into_owned();
            if !prefix.file_type().map_err(io(&path))?.is_dir() || !is_prefix(&name) {
                continue;
            }
            for blob in std::fs::read_dir(&path).map_err(io(&path))? {
                let blob = blob.map_err(io(&path))?;
                let digest = format!("{name}{}", blob.file_name().to_string_lossy());
                if blob.file_type().map_err(io(&blob.path()))?.is_file()
                    && let Ok(a) = ArtifactRef::from_digest(&digest)
                {
                    all.insert(a);
                }
            }
        }
        Ok(all)
    }

    /// Everything any store holds now, whether that store is open or not:
    /// a sweep's caller reads this before it reads what records reference,
    /// since a hold may be let go once its reference is recorded. Takes no
    /// lock. A hold it cannot read fails it.
    pub fn held(&self) -> Result<BTreeSet<ArtifactRef>, ArtifactError> {
        let (digests, _) = holds(
            self.held_dir(),
            self.owners(),
            &[],
            std::time::Duration::MAX,
            std::time::SystemTime::now(),
        )?;
        Ok(digests
            .iter()
            .filter_map(|d| ArtifactRef::from_digest(d).ok())
            .collect())
    }

    /// Mark `artifact` as just written, so a sweep's grace covers it,
    /// without waiting for the store's lock.
    pub fn refresh(&self, artifact: &ArtifactRef) -> Result<Refreshed, ArtifactError> {
        let _lock = match StoreLock::take(&self.lock, false, std::time::Duration::ZERO) {
            Ok(lock) => lock,
            Err(ArtifactError::Locked { .. }) => return Ok(Refreshed::Held),
            Err(e) => return Err(e),
        };
        let blob = self.blob_path(artifact);
        match std::fs::File::open(&blob) {
            Ok(f) => {
                f.set_modified(std::time::SystemTime::now())
                    .map_err(io(&blob))?;
                Ok(Refreshed::Now)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Refreshed::Missing),
            Err(e) => Err(io(&blob)(e)),
        }
    }

    /// How long a write or a sweep waits for the store's lock.
    pub fn lock_wait(&self) -> std::time::Duration {
        self.lock_wait
    }

    /// The project's store, `<state>/artifacts`, with its lock file in the
    /// project's private directory, which atoms' commands cannot open: an
    /// atom that could take the lock could stall every write.
    pub fn for_project(state: &crate::state::ProjectState) -> Result<Self, ArtifactError> {
        Self::open_locked_at(
            state.dir().join("artifacts"),
            state.private_dir().join("artifacts.lock"),
            STAGING_GRACE,
        )
    }

    /// The store at `root` with its lock at `lock`, and its owners beside
    /// the lock, where the lock is kept from atoms' commands too.
    fn open_locked_at(
        root: PathBuf,
        lock: PathBuf,
        grace: std::time::Duration,
    ) -> Result<Self, ArtifactError> {
        for d in ["blobs/sha256", "manifests/sha256", "staging"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(io(&p))?;
        }
        let owners = lock.with_file_name("artifact-owners");
        std::fs::create_dir_all(&owners).map_err(io(&owners))?;
        let held = lock.with_file_name("artifact-held");
        let owner = std::sync::Arc::new(Owner::take(&owners, &held)?);
        sweep_staging(&owners, &held, &root.join("staging"), grace);
        Ok(Self {
            root,
            lock,
            lock_wait: LOCK_WAIT,
            owner,
        })
    }

    /// Where the owners of this store's staging files are.
    fn owners(&self) -> &Path {
        self.owner.path.parent().unwrap_or(&self.root)
    }

    /// Where every open store's holds are.
    fn held_dir(&self) -> &Path {
        self.owner.held.parent().unwrap_or(&self.root)
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

/// Make `f` writable by no one, keeping who may read it.
fn read_only(f: &std::fs::File) -> std::io::Result<()> {
    let mut permissions = f.metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        if mode & 0o222 == 0 {
            return Ok(());
        }
        permissions.set_mode(mode & !0o222);
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

/// What [`LocalStore::refresh`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refreshed {
    /// The content is marked as just written.
    Now,
    /// A sweep holds the store; nothing was done.
    Held,
    /// The store does not hold it.
    Missing,
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

/// The private directory of the project whose store is `root`, when `root`
/// is a project's store in an atomic home: `<home>/projects/<id>/artifacts`
/// has `<home>/private/<id>`. A home is the configured one, or one that
/// already has its `private/`.
fn project_private(root: &Path) -> Option<PathBuf> {
    if root.file_name()? != "artifacts" {
        return None;
    }
    let state = root.parent()?;
    let projects = state.parent()?;
    if projects.file_name()? != "projects" {
        return None;
    }
    let home = projects.parent()?;
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let configured = crate::state::base_dir().is_ok_and(|b| real(&b) == real(home));
    if !configured && !home.join("private").is_dir() {
        return None;
    }
    Some(home.join("private").join(state.file_name()?))
}

/// Make `dir`, readable by its owner only, if it is not there; refuse one
/// that is there but is not a directory, or that others may read or enter.
/// An existing directory is never changed.
fn own_private_dir(dir: &Path) -> Result<(), ArtifactError> {
    let refuse = |why: String| ArtifactError::Io {
        path: dir.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, why),
    };
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(dir).map_err(io(dir))
        }
        Err(e) => Err(io(dir)(e)),
        Ok(meta) if !meta.is_dir() => Err(refuse(
            "the project's private directory is not a directory; the store's lock is not \
             kept there"
                .into(),
        )),
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(refuse(format!(
                        "the project's private directory is open to others (mode {:o}); the \
                         store's lock is not kept there",
                        mode & 0o777
                    )));
                }
            }
            #[cfg(not(unix))]
            let _ = meta;
            Ok(())
        }
    }
}

/// Whether `name` is a two-hex-digit prefix directory of the blob tree.
fn is_prefix(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl LocalStore {
    /// Remove every blob, and its manifests, that `keep` does not name, that
    /// no open store [holds](ArtifactStore::hold), and that was last written more than `grace` before `read_at`, the time the
    /// caller began reading what to keep; and partial writes as old whose
    /// store is no longer open. Entries
    /// in the blob tree that are not content (a stray file, a prefix
    /// directory that is a link) are left alone and reported in
    /// [`Swept::skipped`].
    ///
    /// Content written after the caller began reading is kept, however long
    /// the reading and the wait for the lock took: a reference to it may have
    /// been recorded after the caller read past it. It holds the store's
    /// lock while it runs, so a write waits for it and another sweep runs
    /// after it; content a write kept or added is then as recent as that
    /// write. What it keeps is the caller's to say: a sweep keeps nothing
    /// another caller needs unless that is in `keep`, held or recent.
    pub fn sweep(
        &self,
        keep: &BTreeSet<ArtifactRef>,
        grace: Grace,
        read_at: std::time::SystemTime,
    ) -> Result<Swept, ArtifactError> {
        let _lock = StoreLock::take(&self.lock, true, self.lock_wait)?;
        // Who is open, and what they hold, before any blob is judged: a hold
        // is written under the store's shared lock, so none is taken while
        // this sweep runs.
        let now = std::time::SystemTime::now();
        let (live, dead) = live_owners(self.owners(), STAGING_GRACE, now);
        let (held, gone) = holds(self.held_dir(), self.owners(), &dead, STAGING_GRACE, now)?;
        let cutoff = read_at
            .checked_sub(grace.0)
            .unwrap_or(std::time::UNIX_EPOCH);
        let old = |path: &Path| -> Result<bool, ArtifactError> {
            let written = std::fs::symlink_metadata(path)
                .and_then(|m| m.modified())
                .map_err(io(path))?;
            Ok(written < cutoff)
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
                if keep.contains(&artifact)
                    || held.contains(artifact.digest())
                    || !old(&blob.path())?
                {
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
        // A partial write is left while its store is open, however long it
        // has taken: a gather stages every file before it commits any.
        let staging = self.root.join("staging");
        for partial in std::fs::read_dir(&staging).map_err(io(&staging))?.flatten() {
            let name = partial.file_name().to_string_lossy().into_owned();
            if !live.contains(staged_by(&name)) && old(&partial.path())? {
                let _ = std::fs::remove_file(partial.path());
                swept.partial += 1;
            }
        }
        // The holds of stores that are gone, while the dead are locked.
        for path in gone {
            let _ = std::fs::remove_dir_all(path);
        }
        drop(dead);
        swept.removed.sort();
        Ok(swept)
    }
}

impl ArtifactStore for LocalStore {
    fn lock_wait(&self) -> std::time::Duration {
        self.lock_wait
    }

    fn stage(&self, content: &mut dyn Read) -> Result<Staged, ArtifactError> {
        // Stream to staging while hashing; committing moves it into place
        // under its digest. A sweep leaves it while this store is open.
        let (tmp, mut f) = self.staging_file()?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let written = (|| {
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
        let staged = Staged {
            path: tmp,
            digest: format!("{:x}", hasher.finalize()),
            size,
        };
        // A failed write's file goes as `staged` is dropped.
        written.map(|()| staged)
    }

    fn commit(
        &self,
        mut staged: Staged,
        provenance: &[Provenance],
    ) -> Result<Vec<Manifest>, ArtifactError> {
        // Into place under its digest, with no sweep in between. What was
        // staged and not moved goes as `staged` is dropped, however this
        // ends.
        let _lock = StoreLock::take(&self.lock, false, self.lock_wait)?;
        let (digest, size) = (staged.digest.clone(), staged.size);
        let artifact = ArtifactRef::from_digest(&digest).map_err(|e| ArtifactError::Io {
            path: staged.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        let blob = self.blob_path(&artifact);
        let parent = blob.parent().map(Path::to_owned).unwrap_or_default();
        if intact(&blob, &digest, size)? {
            // Already stored: identical content is kept once, and is now as
            // recent as this write, so a sweep's grace period covers it. The
            // blob is opened read-only (its owner may set its times). The
            // time is not synced: after a crash it may be older, and the
            // content is then only as safe as the caller's keep set. A blob
            // an older atomic left writable is made read-only too.
            std::fs::File::open(&blob)
                .and_then(|f| {
                    f.set_modified(std::time::SystemTime::now())?;
                    read_only(&f)
                })
                .map_err(io(&blob))?;
        } else {
            // New, or a blob that no longer holds its content: the content
            // just written takes its place, as recent as this commit rather
            // than its staging, which may have been long before, so a sweep
            // that began reading before now keeps it.
            std::fs::File::open(&staged.path)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()))
                .map_err(io(&staged.path))?;
            std::fs::create_dir_all(&parent).map_err(io(&parent))?;
            std::fs::rename(&staged.path, &blob).map_err(io(&blob))?;
            staged.take();
            sync_dir(&parent)?;
        }
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

    fn hold(&self, artifact: &ArtifactRef) -> Result<Hold, ArtifactError> {
        // Written under the store's shared lock, so a sweep, which reads the
        // holds under it alone, either finished before or sees this one.
        let _ordered = StoreLock::take(&self.lock, false, self.lock_wait)?;
        // Named after the content, so a sweep reads the digest from the
        // name; each hold its own, so letting one go keeps the others.
        let dir = &self.owner.held;
        std::fs::create_dir_all(dir).map_err(io(dir))?;
        let path = dir.join(format!("{}-{}", artifact.digest(), random_name()?));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io(&path))?;
        Ok(Hold { path })
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
