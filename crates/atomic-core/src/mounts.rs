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
//! denied too.
//!
//! What it cannot see is a copy, or another filesystem built over one: a
//! snapshot holds its own copy of what was there when it was taken, and an
//! overlay or FUSE filesystem that shows a denied directory's files is a
//! filesystem of its own, not a second mount of the denied one.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::bail;
use std::path::{Path, PathBuf};

/// One line of a mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    /// The filesystem's device, `major:minor`.
    device: String,
    /// The directory of the filesystem the mount shows.
    root: PathBuf,
    /// Where it is mounted.
    at: PathBuf,
}

/// A path field of the mount table with its octal escapes (`\040` for a
/// space, `\011`, `\012`, `\134`) undone; `None` for one that cannot be
/// read.
fn unescape(field: &str) -> Option<PathBuf> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes.get(i + 1..i + 4)?;
            if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                return None;
            }
            let value = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value).ok()?);
            i += 4;
        } else {
            out.push(bytes[i]);
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
fn parse(mountinfo: &str) -> anyhow::Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    for (n, line) in mountinfo.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        let read = || -> Option<Mount> {
            let device = (*fields.get(2)?).to_owned();
            if !device.contains(':') {
                return None;
            }
            let root = unescape(fields.get(3)?)?;
            let at = unescape(fields.get(4)?)?;
            // The optional fields end with a lone `-`.
            fields.iter().skip(6).position(|f| *f == "-")?;
            (root.is_absolute() && at.is_absolute()).then_some(Mount { device, root, at })
        };
        let Some(mount) = read() else {
            bail!(
                "line {} of the mount table cannot be read, so atomic cannot tell where else \
                 the places it denies are mounted: {line:?}",
                n + 1
            );
        };
        mounts.push(mount);
    }
    Ok(mounts)
}

/// The mount `path` is in: the last listed of those at the longest prefix
/// of it, since a later mount at a place covers an earlier one.
fn mount_of<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.at))
        .fold(None, |best: Option<&Mount>, m| match best {
            Some(b) if b.at.components().count() > m.at.components().count() => Some(b),
            _ => Some(m),
        })
}

/// Every other place `mounts` shows each of `denied`, which are absolute
/// paths with no link in them: under each other mount of the same
/// filesystem whose directory holds it, the same file or directory there;
/// and each such mount whose directory lies inside it, whole.
fn aliases(mounts: &[Mount], denied: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in denied {
        let Some(home) = mount_of(mounts, path) else {
            bail!(
                "{} is under no mount in the mount table, so atomic cannot tell where else it \
                 is mounted",
                path.display()
            );
        };
        let Ok(rel) = path.strip_prefix(&home.at) else {
            continue;
        };
        let inside = home.root.join(rel);
        for other in mounts.iter().filter(|m| m.device == home.device) {
            let alias = if let Ok(below) = inside.strip_prefix(&other.root) {
                other.at.join(below)
            } else if other.root.starts_with(&inside) {
                other.at.clone()
            } else {
                continue;
            };
            if alias != *path {
                out.push(alias);
            }
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

/// Every other place the mount table shows each of `denied`: each must be
/// denied too. On Linux the table is `/proc/self/mountinfo`, the mounts
/// this process and the sandbox it starts see; one that cannot be read
/// fails it rather than leave a place undenied. Elsewhere there is no such
/// table, and none is found.
pub fn second_mounts(denied: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/self/mountinfo").context(
            "read /proc/self/mountinfo, to find where else the places atomic denies are mounted",
        )?;
        let mounts = parse(&table)?;
        let real: Vec<PathBuf> = denied.iter().map(|p| real(p)).collect();
        aliases(&mounts, &real)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (denied, parse, aliases, real);
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
22 1 0:21 /@ / rw,relatime shared:1 - btrfs /dev/sda2 rw,subvol=/@
23 22 0:21 /@home /home rw,relatime shared:2 - btrfs /dev/sda2 rw,subvol=/@home
24 22 0:21 / /mnt/top rw,relatime shared:3 - btrfs /dev/sda2 rw,subvol=/
25 22 0:21 /@home/u /run/host/home/u rw,relatime - btrfs /dev/sda2 rw
26 22 0:21 /@home/u/.ssh/keys /srv/keys rw,relatime - btrfs /dev/sda2 rw
27 22 0:30 / /tmp rw shared:4 - tmpfs tmpfs rw
28 22 0:21 /@home/v\\040w /mnt/v\\040w rw - btrfs /dev/sda2 rw
";

    fn table() -> Vec<Mount> {
        parse(TABLE).expect("parses")
    }

    #[test]
    fn a_denied_path_is_found_under_every_mount_that_shows_it() {
        let found = aliases(&table(), &[PathBuf::from("/home/u/.ssh")]).expect("aliases");
        assert_eq!(
            found,
            [
                "/mnt/top/@home/u/.ssh",
                "/run/host/home/u/.ssh",
                "/srv/keys",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn another_filesystem_and_an_unrelated_subtree_are_not_aliases() {
        assert!(
            aliases(&table(), &[PathBuf::from("/tmp/x")])
                .expect("aliases")
                .is_empty()
        );
        // /@ is a sibling of /@home, not a prefix of it.
        let found = aliases(&table(), &[PathBuf::from("/etc/secret")]).expect("aliases");
        assert_eq!(found, [PathBuf::from("/mnt/top/@/etc/secret")]);
    }

    #[test]
    fn escaped_paths_are_read_as_they_are() {
        let found = aliases(&table(), &[PathBuf::from("/home/v w/.netrc")]).expect("aliases");
        assert!(
            found.contains(&PathBuf::from("/mnt/v w/.netrc")),
            "{found:?}"
        );
    }

    #[test]
    fn a_table_it_cannot_read_fails_closed() {
        for bad in [
            "22 1 0:21 / / rw - ext4 /dev/sda1 rw\n23 x\n",
            "22 1 nodevice / / rw - ext4 /dev/sda1 rw\n",
            "22 1 0:21 rel / rw - ext4 /dev/sda1 rw\n",
            "22 1 0:21 / /a\\09 rw - ext4 /dev/sda1 rw\n",
            "22 1 0:21 / / rw ext4 /dev/sda1 rw\n",
        ] {
            assert!(
                parse(bad).is_err_and(|e| e.to_string().contains("cannot be read")),
                "{bad:?}"
            );
        }
        assert!(
            aliases(&[], &[PathBuf::from("/home/u")])
                .is_err_and(|e| e.to_string().contains("under no mount")),
        );
    }
}
