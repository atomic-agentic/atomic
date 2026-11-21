// SPDX-License-Identifier: Apache-2.0

//! The other places a denied path can be read through.
//!
//! A directory can be mounted at more than one place: a bind mount of a
//! home elsewhere, the host's files under `/run/host` inside toolbox or
//! distrobox, or a btrfs filesystem's top-level subvolume mounted beside the
//! subvolume that holds the home. The sandbox denies a path where it is
//! named, so what it names is still readable through each other mount of
//! the same filesystem that shows it. [`second_mounts`] reads the mount
//! table, `/proc/self/mountinfo`, and finds every such place, so it is
//! denied too; and so for each filesystem mounted inside a denied
//! directory, whose files the denial hides there too.
//!
//! It refuses (an error, so nothing runs) when it cannot tell: a table or a
//! line it cannot read, one with a mount on a mount it does not list, a
//! denied path it finds no mount for, a denied path on NFS or SMB
//! ([`unknowable`]) that is mounted more than once, since the table cannot
//! be trusted to say which directory each such mount shows, or one on a
//! FUSE, 9p or virtiofs filesystem mounted more than once whose root the
//! kernel has lost ([`may_lose_roots`]). A mount
//! whose root is not a path, such as a namespace file (`net:[4026531840]`),
//! shows no directory, and is never a second place.
//!
//! What it cannot see:
//!
//! - a copy: a snapshot holds its own copy of what was there when it was
//!   taken;
//! - another filesystem built over a denied directory: an overlay whose
//!   upper or lower directory is inside one, or a FUSE filesystem that
//!   serves its files, shows them at its own mount point as files of its
//!   own;
//! - a mount made after the list is built, such as one an automount
//!   (`autofs`) makes when a path under it is first used, even from inside
//!   the sandbox, which sees the host's new mounts;
//! - the same remote directory, or one inside another, mounted twice as
//!   two filesystems, each with a device of its own, such as 9p; NFS with
//!   `nosharecache`, or with mount options that differ, which give each
//!   mount a superblock of its own; Ceph, which has one per server path;
//!   two SMB sessions; or two FUSE daemons serving the same files. Neither
//!   is a second mount of the other, so neither is denied for the other,
//!   nor refused.

use anyhow::bail;
use std::path::{Path, PathBuf};

/// One line of a mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    /// Its ID.
    id: u64,
    /// The ID of the mount it is mounted on.
    parent: u64,
    /// The filesystem's device, `major:minor`.
    device: String,
    /// The directory of the filesystem the mount shows; `None` when the
    /// root is not a path, as a namespace file's is not.
    root: Option<PathBuf>,
    /// Where it is mounted.
    at: PathBuf,
    /// The filesystem's type, such as `ext4` or `nfs4`.
    kind: String,
    /// Whether the table shows the root as lost: `<dir>//deleted`.
    lost: bool,
}

/// Whether a filesystem of type `kind` is one whose mounts' roots in the
/// table cannot be trusted to say what directory each shows: NFS, which
/// gives every mount `/`, and SMB, whose root is found by walking from the
/// share's and is the least known. FUSE, 9p and virtiofs show the directory
/// a bind of part of them shows, as local filesystems do, unless the kernel
/// has lost it ([`may_lose_roots`]).
fn unknowable(kind: &str) -> bool {
    matches!(kind, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs")
}

/// Whether a filesystem of type `kind` is one whose mount's root the kernel
/// shows as `<dir>//deleted` once a lookup of it fails, as a FUSE
/// filesystem's does when its server is away: the directory it names is not
/// the one the mount shows.
fn may_lose_roots(kind: &str) -> bool {
    matches!(
        kind,
        "fuse" | "fuseblk" | "virtiofs" | "9p" | "cifs" | "smb3"
    ) || kind.starts_with("fuse.")
}

/// A path field of the mount table with its octal escapes (`\040` for a
/// space, `\011`, `\012`, `\134`) undone; `None` for one that cannot be
/// read.
fn unescape(field: &[u8]) -> Option<PathBuf> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' {
            let digits = field.get(i + 1..i + 4)?;
            if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                return None;
            }
            let value = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value).ok()?);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(out).ok().map(PathBuf::from)
    }
}

/// The mounts a `mountinfo` table lists, in its order. A line it cannot read
/// fails it: a mount it left out could be one that shows a denied path.
fn parse(mountinfo: &[u8]) -> anyhow::Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    for (n, line) in mountinfo.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let fields: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        let read = || -> Option<Mount> {
            let number = |f: &[u8]| std::str::from_utf8(f).ok()?.parse::<u64>().ok();
            let id = number(fields.first()?)?;
            let parent = number(fields.get(1)?)?;
            let device = std::str::from_utf8(fields.get(2)?).ok()?;
            let (major, minor) = device.split_once(':')?;
            if major.parse::<u32>().is_err() || minor.parse::<u32>().is_err() {
                return None;
            }
            // Before `//` is collapsed by reading it as a path.
            let lost = fields.get(3)?.ends_with(b"//deleted");
            let root = unescape(fields.get(3)?)?;
            let at = unescape(fields.get(4)?)?;
            // The optional fields end with a lone `-`, then the type.
            let dash = 6 + fields.iter().skip(6).position(|f| *f == b"-")?;
            let kind = std::str::from_utf8(fields.get(dash + 1)?).ok()?;
            if kind.is_empty() || !at.is_absolute() || root.as_os_str().is_empty() {
                return None;
            }
            Some(Mount {
                id,
                parent,
                device: device.to_owned(),
                root: root.is_absolute().then_some(root),
                at,
                kind: kind.to_owned(),
                lost,
            })
        };
        let Some(mount) = read() else {
            bail!(
                "line {} of the mount table cannot be read, so atomic cannot tell where else \
                 the places it denies are mounted: {:?}",
                n + 1,
                String::from_utf8_lossy(line)
            );
        };
        mounts.push(mount);
    }
    // Every mount but the root is mounted on one the table lists: one that
    // is not was cut from the table, or the table is not one.
    let listed = ids(&mounts);
    if let Some(root) = root_of(&mounts, &listed) {
        let ids = listed;
        if let Some(orphan) = mounts
            .iter()
            .find(|m| m.id != root.id && !ids.contains(&m.parent))
        {
            bail!(
                "the mount table lists {} on mount {}, which it does not list, so atomic cannot \
                 tell where else the places it denies are mounted",
                orphan.at.display(),
                orphan.parent
            );
        }
    } else if !mounts.is_empty() {
        bail!(
            "the mount table lists no mount at / that is mounted on nothing it lists, as when \
             atomic runs under a chroot whose root is not a mount point, so atomic cannot tell \
             where else the places it denies are mounted"
        );
    }
    Ok(mounts)
}

/// The mount at the top of the stack `mount` heads at its own mount point:
/// the one mounted on it there, and on that, and so on.
fn top<'a>(mounts: &'a [Mount], mut mount: &'a Mount) -> &'a Mount {
    for _ in 0..mounts.len() {
        match mounts
            .iter()
            .find(|m| m.at == mount.at && m.parent == mount.id && m.id != mount.id)
        {
            Some(above) => mount = above,
            None => break,
        }
    }
    mount
}

/// The IDs of `mounts`.
fn ids(mounts: &[Mount]) -> std::collections::HashSet<u64> {
    mounts.iter().map(|m| m.id).collect()
}

/// The root mount: the one at `/` mounted on nothing listed (`ids`), or on
/// itself, as an initramfs's root is.
fn root_of<'a>(mounts: &'a [Mount], ids: &std::collections::HashSet<u64>) -> Option<&'a Mount> {
    mounts
        .iter()
        .find(|m| m.at == Path::new("/") && (m.parent == m.id || !ids.contains(&m.parent)))
}

/// The mount `path` is in, as this process sees it: from the root mount,
/// down the mounts on each directory of the path, each the top of its stack.
fn mount_of<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    let root = root_of(mounts, &ids(mounts))?;
    let mut here = top(mounts, root);
    let mut at = PathBuf::from("/");
    for part in path.components().skip(1) {
        at.push(part);
        if let Some(child) = mounts.iter().find(|m| m.at == at && m.parent == here.id) {
            here = top(mounts, child);
        }
    }
    Some(here)
}

/// Every other place `mounts` shows `path`, which is absolute with no link
/// in it: under each other mount of the same filesystem whose directory
/// holds it, the same file or directory there; and each such mount whose
/// directory lies inside it, whole.
fn aliases_of(mounts: &[Mount], path: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let Some(home) = mount_of(mounts, path) else {
        bail!(
            "{} is under no mount in the mount table, so atomic cannot tell where else it is \
             mounted",
            path.display()
        );
    };
    let same: Vec<&Mount> = mounts
        .iter()
        .filter(|m| m.device == home.device && m.id != home.id)
        .collect();
    let Some(first) = same.first() else {
        return Ok(());
    };
    if may_lose_roots(&home.kind)
        && let Some(lost) = std::iter::once(home)
            .chain(same.iter().copied())
            .find(|m| m.lost)
    {
        bail!(
            "{} is on the {} filesystem seen at {}, that is mounted again at {}, and the kernel \
             no longer knows which directory the mount at {} shows, so atomic cannot deny it at \
             each",
            path.display(),
            home.kind,
            home.at.display(),
            first.at.display(),
            lost.at.display()
        );
    }
    if unknowable(&home.kind) {
        bail!(
            "{} is on the {} filesystem seen at {}, that is mounted again at {}, and the mount \
             table cannot be trusted to say which directory each of its mounts shows, so atomic \
             cannot deny it at each",
            path.display(),
            home.kind,
            home.at.display(),
            first.at.display()
        );
    }
    let (Some(root), Ok(rel)) = (&home.root, path.strip_prefix(&home.at)) else {
        return Ok(());
    };
    let inside = root.join(rel);
    for other in same {
        let Some(other_root) = &other.root else {
            continue;
        };
        let alias = if let Ok(below) = inside.strip_prefix(other_root) {
            if below.as_os_str().is_empty() {
                other.at.clone()
            } else {
                other.at.join(below)
            }
        } else if other_root.starts_with(&inside) {
            other.at.clone()
        } else {
            continue;
        };
        if alias != path {
            out.push(alias);
        }
    }
    Ok(())
}

/// Every other place `mounts` shows each of `denied`, which are absolute
/// paths with no link in them, and each filesystem mounted inside one.
fn aliases(mounts: &[Mount], denied: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in denied {
        aliases_of(mounts, path, &mut out)?;
        // What is mounted inside a denied directory is hidden there too.
        for inner in mounts
            .iter()
            .filter(|m| m.at != *path && m.at.starts_with(path))
        {
            aliases_of(mounts, &inner.at, &mut out)?;
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `path` with every link in the part of it that exists resolved, and the
/// rest as it is: where a denied path that does not exist yet would be.
fn real(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The mount table [`second_mounts`] reads on this thread, in place of
    /// this process's.
    static TABLE: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

/// Read `table` as this thread's mount table in [`second_mounts`] until the
/// guard returned is dropped.
#[cfg(test)]
pub(crate) fn with_table(table: &[u8]) -> TableGuard {
    TABLE.with(|t| *t.borrow_mut() = Some(table.to_vec()));
    TableGuard
}

/// Puts this process's own mount table back for [`second_mounts`] when
/// dropped.
#[cfg(test)]
#[must_use = "dropping the guard puts this process's own table back at once"]
pub(crate) struct TableGuard;

#[cfg(test)]
impl Drop for TableGuard {
    fn drop(&mut self) {
        TABLE.with(|t| *t.borrow_mut() = None);
    }
}

/// For each group of `groups`, every other place the mount table shows
/// each of its paths, and each filesystem mounted inside one: each must be
/// denied too. The table is read once for all of them. On Linux it is
/// `/proc/self/mountinfo`, the mounts this process and the sandbox it starts
/// see; one that cannot be read, or that leaves a place it cannot tell
/// about, fails it rather than leave a place undenied. Elsewhere there is no
/// such table, and none is found.
pub fn second_mounts(groups: &[&[PathBuf]]) -> anyhow::Result<Vec<Vec<PathBuf>>> {
    let table: Option<Vec<u8>> = {
        #[cfg(test)]
        let injected = TABLE.with(|t| t.borrow().clone());
        #[cfg(not(test))]
        let injected: Option<Vec<u8>> = None;
        match injected {
            Some(table) => Some(table),
            None => {
                #[cfg(target_os = "linux")]
                {
                    use anyhow::Context;
                    Some(std::fs::read("/proc/self/mountinfo").context(
                        "read /proc/self/mountinfo, to find where else the places atomic \
                         denies are mounted",
                    )?)
                }
                #[cfg(not(target_os = "linux"))]
                None
            }
        }
    };
    let Some(table) = table else {
        return Ok(groups.iter().map(|_| Vec::new()).collect());
    };
    let mounts = parse(&table)?;
    groups
        .iter()
        .map(|group| {
            let real: Vec<PathBuf> = group.iter().map(|p| real(p)).collect();
            aliases(&mounts, &real)
        })
        .collect()
}

#[cfg(test)]
mod tests;
