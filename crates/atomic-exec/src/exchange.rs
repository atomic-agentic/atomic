// SPDX-License-Identifier: Apache-2.0

//! The files an exec atom's command is given and hands back.
//!
//! A command run with an artifact store has two directories in its
//! workspace's scratch directory, the one directory a confined command may
//! write:
//!
//! - its inputs directory (`ATOMIC_INPUTS`), where the artifacts its input
//!   references are put before it runs, each read-only, and which is then
//!   made read-only itself;
//! - its outputs directory (`ATOMIC_OUTPUTS`), where it writes the files it
//!   hands over.
//!
//! The control plane reaches them only through the [`Exchange`] the
//! workspace's backend gives ([`crate::ExecutionBackend::exchange`]), so it
//! never touches a workspace's files itself, and a backend whose runners
//! are elsewhere moves the bytes its own way.
//!
//! On this machine ([`LocalExchange`]), the outputs directory is opened
//! before the command runs and read through that handle afterwards, so
//! swapping it for a link leads nowhere. A file is opened beneath it without
//! following links, not waiting on a FIFO and not taking a terminal, and must
//! be a regular file with one link.

use std::io::Read;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// A command's inputs and outputs directories, as its backend reaches them.
///
/// Every method may block: callers on an async executor call it on the
/// blocking pool.
pub trait Exchange: Send + Sync {
    /// The outputs directory, as the runner sees it: where the command
    /// writes the files it hands over.
    fn outputs(&self) -> &Path;

    /// The inputs directory, as the runner sees it: where the command reads
    /// what it is given.
    fn inputs(&self) -> &Path;

    /// Put what `content` reads in the inputs directory as a new file,
    /// `name`, which the command may only read. A file not written whole is
    /// removed, and the error that stopped it, such as one `content` gave,
    /// is returned as it was. `name` is one plain file name.
    fn give(&self, name: &str, content: &mut dyn Read) -> std::io::Result<()>;

    /// Make the inputs directory read-only, once everything is given.
    fn seal(&self) -> std::io::Result<()>;

    /// Open the file `name` the command wrote in its outputs directory, as
    /// it is now, with its size: a regular file of the command's own, never
    /// one a link names. `name` is one plain file name.
    fn take(&self, name: &str) -> Result<Taken, TakeError>;
}

/// A file a command handed over, opened.
pub struct Taken {
    /// Its content.
    pub content: Box<dyn Read + Send>,
    /// Its size when it was opened.
    pub size: u64,
}

impl std::fmt::Debug for Taken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Taken")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Why a file a command names cannot be taken.
#[derive(Debug, thiserror::Error)]
pub enum TakeError {
    /// The name is a link.
    #[error("it is a link")]
    Link,
    /// There is no such file in the outputs directory.
    #[error("it is not in the outputs directory")]
    Missing,
    /// The backend may not read it.
    #[error("it may not be read")]
    Unreadable,
    /// It is not a regular file, such as a directory or a FIFO.
    #[error("it is not a regular file")]
    NotRegular,
    /// It has more than one link, so it may be another file's too.
    #[error("it has {0} links")]
    Links(u64),
    /// The name is not one plain file name.
    #[error("it is not one file name")]
    Name,
    /// Something else failed.
    #[error(transparent)]
    Io(std::io::Error),
}

/// Whether `name` is one plain file name: not empty, not `.` or `..`, and
/// with no separator or NUL.
#[cfg(unix)]
fn plain(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']))
}

#[cfg(unix)]
fn refuse_name(name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{name:?} is not one file name"),
    )
}

/// The exchange of a workspace on this machine, in its scratch directory.
/// Unix only: elsewhere the local backend hands no files over.
#[cfg(unix)]
#[derive(Debug)]
pub struct LocalExchange {
    outputs: PathBuf,
    inputs: PathBuf,
    /// The outputs directory, opened before the command ran.
    outputs_dir: std::fs::File,
}

#[cfg(unix)]
impl LocalExchange {
    /// Make the two directories in `scratch`, private and new, and open the
    /// outputs one, before the command runs.
    pub fn create(scratch: &Path) -> std::io::Result<Self> {
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
}

#[cfg(unix)]
impl Exchange for LocalExchange {
    fn outputs(&self) -> &Path {
        &self.outputs
    }

    fn inputs(&self) -> &Path {
        &self.inputs
    }

    fn give(&self, name: &str, content: &mut dyn Read) -> std::io::Result<()> {
        if !plain(name) {
            return Err(refuse_name(name));
        }
        let path = self.inputs.join(name);
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o400);
        let mut file = options.open(&path)?;
        if let Err(e) = std::io::copy(content, &mut file) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        set_mode(&path, 0o444)
    }

    fn seal(&self) -> std::io::Result<()> {
        set_mode(&self.inputs, 0o555)
    }

    fn take(&self, name: &str) -> Result<Taken, TakeError> {
        if !plain(name) {
            return Err(TakeError::Name);
        }
        let file = open_beneath(&self.outputs_dir, name).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => TakeError::Link,
            Some(libc::ENOENT) => TakeError::Missing,
            Some(libc::EACCES) => TakeError::Unreadable,
            _ => TakeError::Io(e),
        })?;
        let meta = file.metadata().map_err(TakeError::Io)?;
        if !meta.is_file() {
            return Err(TakeError::NotRegular);
        }
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(TakeError::Links(meta.nlink()));
        }
        Ok(Taken {
            content: Box::new(file),
            size: meta.len(),
        })
    }
}

/// Make `path` a new private directory (0700).
#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Open the directory at `path` itself, not what a link there names.
#[cfg(unix)]
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
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

/// Set `path`'s mode.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}
