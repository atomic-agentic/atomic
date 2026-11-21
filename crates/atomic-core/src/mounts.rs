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
//! be trusted to say which directory each such mount shows; one on any
//! filesystem but a local one ([`local`]) mounted more than once whose root
//! the kernel shows as lost (`<path>//deleted`); and one there now whose
//! second place, on such a filesystem, is missing, is another file or
//! directory, or is a link (which is not followed), or which, or whose
//! part a second place shows, cannot be looked at for a reason other than
//! not being there, since the table's roots there can be stale. A mount of
//! part of a denied place is checked against that part. On any filesystem,
//! it refuses when another mount of it is the same directory as a denied
//! place, or as one above it on the place's own mount, and the table did
//! not give that second place ([`shown_unlisted`]): its root went stale
//! with nothing to say so. A mount whose
//! root is not a path, such as a namespace file (`net:[4026531840]`), shows
//! no directory, and is never a second place.
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
//!   nor refused;
//! - a stale root on a filesystem that is not local, for a place not there
//!   yet: its second place is only checked against it once it is;
//! - a stale root that makes a mount of a directory inside a denied place
//!   look unrelated to it: mounts are checked against the place and the
//!   directories above it, not each one below it; and a stale root of the
//!   place's own mount, which makes a second mount of a directory above
//!   that root look unrelated: the directories checked stop at the place's
//!   own mount point.

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
    /// root is not a path, as a namespace file's is not. A lost root holds
    /// where it was.
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
/// share's and is the least known. Others show the directory a bind of part
/// of them shows, unless the kernel has lost it (see [`local`]).
fn unknowable(kind: &str) -> bool {
    matches!(kind, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs")
}

/// Whether a filesystem of type `kind` is a local one, on which a root the
/// table shows as `<path>//deleted` was deleted or replaced, as a file a
/// container binds is when it is replaced by a rename: what the mount shows
/// is the old file or directory, at no path now. On every other filesystem
/// (FUSE, 9p, virtiofs, Ceph, AFS, cluster and guest filesystems) the kernel
/// shows that when it can no longer check that a live directory is where
/// it was, such as when its server is away, and the directory it names may
/// not be the one the mount shows.
fn local(kind: &str) -> bool {
    matches!(
        kind,
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "f2fs" | "tmpfs" | "ramfs" | "overlay"
    )
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
            // Before `//` is collapsed by reading it as a path. A lost root
            // names where it was: what is there now is not what it shows.
            let lost = fields.get(3)?.ends_with(b"//deleted");
            let mut root = unescape(fields.get(3)?)?;
            if lost {
                root = root.parent()?.to_owned();
            }
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
        if let Some(orphan) = mounts
            .iter()
            .find(|m| m.id != root.id && !listed.contains(&m.parent))
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
fn mount_of<'a>(
    mounts: &'a [Mount],
    listed: &std::collections::HashSet<u64>,
    path: &Path,
) -> Option<&'a Mount> {
    let root = root_of(mounts, listed)?;
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
fn aliases_of(
    mounts: &[Mount],
    listed: &std::collections::HashSet<u64>,
    path: &Path,
    out: &mut Vec<PathBuf>,
    points: &mut Points,
) -> anyhow::Result<()> {
    let Some(home) = mount_of(mounts, listed, path) else {
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
    // Only off a local filesystem is a lost root one that may name another
    // directory than the mount shows.
    let lost_here = !local(&home.kind);
    let lost = std::iter::once(home)
        .chain(same.iter().copied())
        .find(|m| m.lost);
    if lost_here && let Some(lost) = lost {
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
    let start = out.len();
    if let (Some(root), Ok(rel)) = (&home.root, path.strip_prefix(&home.at)) {
        let inside = root.join(rel);
        for other in same.iter().copied() {
            let Some(other_root) = &other.root else {
                continue;
            };
            // The second place, and what it shows of the place: the place itself,
            // or, for a mount of part of it, that part.
            let (alias, shows) = if let Ok(below) = inside.strip_prefix(other_root) {
                if below.as_os_str().is_empty() {
                    (other.at.clone(), path.to_owned())
                } else {
                    (other.at.join(below), path.to_owned())
                }
            } else if let Ok(part) = other_root.strip_prefix(&inside) {
                (other.at.clone(), path.join(part))
            } else {
                continue;
            };
            if alias == path {
                continue;
            }
            // Off a local filesystem, a root may be stale with nothing in the
            // table to say so: an ancestor of it lost, or renamed on the server
            // and not checked again yet. Where the place is there, its second
            // place must be the same file or directory as what it shows. Where
            // what it shows is not there, it is denied unchecked: a stale root
            // then denies more, never less.
            if !local(&home.kind) {
                same_file(&shows, &alias)?;
            }
            out.push(alias);
        }
    }
    // What the table's roots say, checked against the directories the
    // mounts are of.
    shown_unlisted(home, &same, path, &out[start..], points)?;
    Ok(())
}

/// What each mount point shows, looked at once for each read of the table:
/// the file it is, or `None` when it is not there or out of reach.
type Points = std::collections::HashMap<PathBuf, Option<(u64, u64)>>;

/// Refused when another mount of `home`'s filesystem, of `same`, shows
/// `path` with nothing in the table to say so: its mount point is the same
/// directory as `path` or one above it on `home`, and the table did not
/// yield that second place among `found`. A root can go stale with no
/// marker, as an overlay's does once a lower directory under a bound one is
/// renamed, or a remote filesystem's when a directory above it is renamed
/// on the server; the second mount then looks unrelated to the place. A
/// mount point that is not there shows nothing, and nor does one behind a
/// directory this user may not search that is someone else's, by a uid the
/// user cannot map ([`out_of_reach`]); behind any other, it is refused. A
/// place, or a directory above it, that cannot be looked at for any reason
/// but not being there refuses too: what it shows cannot be checked.
#[cfg(unix)]
fn shown_unlisted(
    home: &Mount,
    same: &[&Mount],
    path: &Path,
    found: &[PathBuf],
    points: &mut Points,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Ok(rel) = path.strip_prefix(&home.at) else {
        return Ok(());
    };
    // The place and each directory above it on its home mount, as the
    // file each is, with what of the place lies below it.
    let mut chain = vec![home.at.clone()];
    for part in rel.components() {
        let next = chain[chain.len() - 1].join(part);
        chain.push(next);
    }
    let mut above = Vec::new();
    for dir in &chain {
        match std::fs::symlink_metadata(dir) {
            Ok(m) => {
                let below = path.strip_prefix(dir).unwrap_or(Path::new("")).to_owned();
                above.push(((m.dev(), m.ino()), dir, below));
            }
            Err(e) if not_there(&e) => break,
            Err(e) => bail!(
                "{} cannot be looked at ({e}), so atomic cannot check which other mounts show \
                 {}; it cannot deny it at each",
                dir.display(),
                path.display()
            ),
        }
    }
    for other in same {
        let shown = match points.get(&other.at) {
            Some(shown) => *shown,
            None => {
                let shown = match std::fs::symlink_metadata(&other.at) {
                    Ok(m) => Some((m.dev(), m.ino())),
                    Err(e) if not_there(&e) => None,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                            && out_of_reach(&other.at) =>
                    {
                        None
                    }
                    Err(e) => bail!(
                        "the mount at {} cannot be looked at ({e}), so atomic cannot check \
                         whether it shows {}; it cannot deny it at each",
                        other.at.display(),
                        path.display()
                    ),
                };
                points.insert(other.at.clone(), shown);
                shown
            }
        };
        let Some(shown) = shown else {
            continue;
        };
        for (id, dir, below) in &above {
            if shown != *id {
                continue;
            }
            let alias = if below.as_os_str().is_empty() {
                other.at.clone()
            } else {
                other.at.join(below)
            };
            if alias == path || found.contains(&alias) {
                continue;
            }
            bail!(
                "the mount at {} shows {}, though its root in the mount table ({}) names no path \
                 to it there: the root is stale, or names another link to the same file; atomic \
                 cannot deny {} at each",
                other.at.display(),
                dir.display(),
                other
                    .root
                    .as_deref()
                    .map_or_else(|| "none".to_owned(), |r| r.display().to_string()),
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn shown_unlisted(
    _: &Mount,
    _: &[&Mount],
    _: &Path,
    _: &[PathBuf],
    _: &mut Points,
) -> anyhow::Result<()> {
    Ok(())
}

/// Whether `path`, which this user may not look at, is out of reach of any
/// command run as this user too: the directory above it that refuses the
/// search is someone else's, by a uid the user cannot map. One of the
/// user's own refuses only by its mode, which a command can step around
/// from a user namespace of its own, where the user is root over what it
/// owns (`unshare -r`); and so, through the user's subordinate uids
/// (`/etc/subuid`, as rootless containers use them), can one the user may
/// have given one of those. The caller then refuses.
#[cfg(unix)]
fn out_of_reach(path: &Path) -> bool {
    let Some(owner) = search_refused_by(path) else {
        return false;
    };
    if owner == this_uid() {
        return false;
    }
    !mappable(owner)
}

/// Whether this user may map `uid` into a user namespace of its own: it is
/// in one of the user's ranges in `/etc/subuid`, named by the user's name
/// or uid. A file there that cannot be read or is not one such counts every
/// uid as mappable; with none, the user maps only itself.
#[cfg(unix)]
fn mappable(uid: u32) -> bool {
    let text = match subuid_file() {
        Ok(text) => text,
        Err(std::io::ErrorKind::NotFound) => return false,
        Err(_) => return true,
    };
    let me = this_uid();
    let name = crate::state::passwd_name(me);
    let ranges: Option<Vec<(String, u64, u64)>> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut fields = line.split(':');
            let (who, start, count) = (fields.next()?, fields.next()?, fields.next()?);
            if fields.next().is_some() {
                return None;
            }
            Some((who.to_owned(), start.parse().ok()?, count.parse().ok()?))
        })
        .collect();
    let Some(ranges) = ranges else {
        return true;
    };
    ranges.iter().any(|(who, start, count)| {
        let mine = *who == me.to_string() || name.as_deref() == Some(who.as_str());
        mine && (*start..start.saturating_add(*count)).contains(&u64::from(uid))
    })
}

/// What `/etc/subuid` holds, or, in a test, what [`with_subuid`] stands in.
#[cfg(unix)]
fn subuid_file() -> Result<String, std::io::ErrorKind> {
    #[cfg(test)]
    if let Some(given) = SUBUID.with(|s| s.borrow().clone()) {
        return given;
    }
    std::fs::read_to_string("/etc/subuid").map_err(|e| e.kind())
}

/// The owner of the directory above `path` that does not let this user
/// search it, found from the root down; `None` when looking fails for any
/// other reason, or nothing refuses.
#[cfg(unix)]
fn search_refused_by(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let mut down: Vec<&Path> = path.ancestors().collect();
    down.reverse();
    for pair in down.windows(2) {
        match std::fs::symlink_metadata(pair[1]) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return std::fs::symlink_metadata(pair[0]).ok().map(|m| m.uid());
            }
            Err(_) => return None,
        }
    }
    None
}

/// This process's user, or, in a test, the one [`with_uid`] stands in.
#[cfg(unix)]
fn this_uid() -> u32 {
    #[cfg(test)]
    if let Some(uid) = UID.with(std::cell::Cell::get) {
        return uid;
    }
    crate::state::effective_uid()
}

#[cfg(test)]
thread_local! {
    static UID: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static SUBUID: std::cell::RefCell<Option<Result<String, std::io::ErrorKind>>> =
        const { std::cell::RefCell::new(None) };
}

/// Have [`this_uid`] answer `uid` on this thread until what it returns is
/// dropped: a test's way to make its own directory someone else's.
#[cfg(test)]
pub(crate) fn with_uid(uid: u32) -> UidGuard {
    UidGuard(UID.with(|u| u.replace(Some(uid))))
}

/// Puts back what [`this_uid`] answered before [`with_uid`] when dropped.
#[cfg(test)]
#[must_use]
pub(crate) struct UidGuard(Option<u32>);

#[cfg(test)]
impl Drop for UidGuard {
    fn drop(&mut self) {
        UID.with(|u| u.set(self.0));
    }
}

/// Have [`subuid_file`] answer `given` on this thread until what it returns
/// is dropped: the file's text, or the error reading it gave.
#[cfg(test)]
pub(crate) fn with_subuid(given: Result<&str, std::io::ErrorKind>) -> SubuidGuard {
    SubuidGuard(SUBUID.with(|s| s.replace(Some(given.map(str::to_owned)))))
}

/// Puts back what [`subuid_file`] answered before [`with_subuid`] when
/// dropped.
#[cfg(test)]
#[must_use]
pub(crate) struct SubuidGuard(Option<Result<String, std::io::ErrorKind>>);

#[cfg(test)]
impl Drop for SubuidGuard {
    fn drop(&mut self) {
        SUBUID.with(|s| *s.borrow_mut() = self.0.take());
    }
}

/// Whether looking at a path failed because nothing is there.
#[cfg(unix)]
fn not_there(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `alias` is the same file or directory as `path`, when `path` is
/// there, neither followed through a link: refused when `alias` is not
/// there, or is another, or either cannot be looked at. Where `path` is not
/// there, nothing is checked and `alias` is denied as it is: so a mount of a
/// part the place does not hold, perhaps from a stale root, is denied too,
/// which denies more, never less.
fn same_file(path: &Path, alias: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let place = match std::fs::symlink_metadata(path) {
            Ok(place) => place,
            // Not there yet: nothing to check it against.
            Err(e) if not_there(&e) => return Ok(()),
            Err(e) => bail!(
                "{} cannot be looked at ({e}), so atomic cannot check that its second mount at {} \
                 shows it; it cannot deny it at each",
                path.display(),
                alias.display()
            ),
        };
        match std::fs::symlink_metadata(alias) {
            Ok(there) if (there.dev(), there.ino()) == (place.dev(), place.ino()) => Ok(()),
            Ok(_) => bail!(
                "{} should show {}, and shows another file or directory, so the mount table's \
                 word for where that filesystem's mounts are is stale; atomic cannot deny it at \
                 each",
                alias.display(),
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "{} should show {}, and is not there, so the mount table's word for where that \
                 filesystem's mounts are is stale; atomic cannot deny it at each",
                alias.display(),
                path.display()
            ),
            Err(e) => bail!(
                "{} should show {}, and cannot be looked at ({e}); atomic cannot deny it at each",
                alias.display(),
                path.display()
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, alias);
        Ok(())
    }
}

/// Every other place `mounts` shows each of `denied`, which are absolute
/// paths with no link in them, and each filesystem mounted inside one.
#[cfg(test)]
fn aliases(mounts: &[Mount], denied: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    aliases_with(mounts, denied, &mut Points::new())
}

/// [`aliases`], with what the mount points show kept in `points`.
fn aliases_with(
    mounts: &[Mount],
    denied: &[PathBuf],
    points: &mut Points,
) -> anyhow::Result<Vec<PathBuf>> {
    let listed = ids(mounts);
    let mut out = Vec::new();
    for path in denied {
        aliases_of(mounts, &listed, path, &mut out, points)?;
        // What is mounted inside a denied directory is hidden there too.
        for inner in mounts
            .iter()
            .filter(|m| m.at != *path && m.at.starts_with(path))
        {
            aliases_of(mounts, &listed, &inner.at, &mut out, points)?;
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
    let mut points = Points::new();
    groups
        .iter()
        .map(|group| {
            let real: Vec<PathBuf> = group.iter().map(|p| real(p)).collect();
            aliases_with(&mounts, &real, &mut points)
        })
        .collect()
}

#[cfg(test)]
mod tests;
