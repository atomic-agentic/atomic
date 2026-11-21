// SPDX-License-Identifier: Apache-2.0

use super::*;

/// A btrfs home with its top-level subvolume mounted beside it, a bind of
/// the home under `/run/host`, a mount of part of a denied directory
/// elsewhere, a tmpfs, a namespace file and a name with a space.
const TABLE: &[u8] = b"\
22 1 0:21 /@ / rw,relatime shared:1 - btrfs /dev/sda2 rw,subvol=/@
23 22 0:21 /@home /home rw,relatime shared:2 - btrfs /dev/sda2 rw,subvol=/@home
24 22 0:21 / /mnt/top rw,relatime shared:3 - btrfs /dev/sda2 rw,subvol=/
25 22 0:21 /@home/u /run/host/home/u rw,relatime - btrfs /dev/sda2 rw
26 22 0:21 /@home/u/.ssh/keys /srv/keys rw,relatime - btrfs /dev/sda2 rw
27 22 0:30 / /tmp rw shared:4 - tmpfs tmpfs rw
28 22 0:21 /@home/v\\040w /mnt/v\\040w rw - btrfs /dev/sda2 rw
29 27 0:4 net:[4026531840] /tmp/netns rw - nsfs nsfs rw
";

fn table() -> Vec<Mount> {
    parse(TABLE).expect("parses")
}

fn found(mounts: &[Mount], denied: &str) -> Vec<PathBuf> {
    aliases(mounts, &[PathBuf::from(denied)]).expect("aliases")
}

#[test]
fn a_denied_path_is_found_under_every_mount_that_shows_it() {
    assert_eq!(
        found(&table(), "/home/u/.ssh"),
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
    assert!(found(&table(), "/tmp/x").is_empty());
    // /@ is a sibling of /@home, not a prefix of it.
    assert_eq!(
        found(&table(), "/etc/secret"),
        [PathBuf::from("/mnt/top/@/etc/secret")]
    );
}

#[test]
fn escaped_paths_are_read_as_they_are() {
    let aliases = found(&table(), "/home/v w/.netrc");
    assert!(
        aliases.contains(&PathBuf::from("/mnt/v w/.netrc")),
        "{aliases:?}"
    );
}

/// A namespace file's root is not a path: it is read, and shows no
/// directory, so it is never a second place, even on the device of a
/// denied one.
#[test]
fn a_namespace_file_is_read_and_never_an_alias() {
    let mounts = table();
    let netns = mounts.iter().find(|m| m.kind == "nsfs").expect("nsfs");
    assert_eq!(netns.root, None);
    let shared: Vec<Mount> = vec![
        Mount {
            id: 1,
            parent: 0,
            device: "0:4".into(),
            root: Some("/".into()),
            at: "/".into(),
            kind: "tmpfs".into(),
            lost: false,
        },
        Mount {
            device: "0:4".into(),
            ..netns.clone()
        },
    ];
    assert!(found(&shared, "/home/u/.ssh").is_empty());
}

/// This kind of machine's own table, as it reads, namespace files, an
/// automount, FUSE mounts and container overlays among its lines, and a
/// home whose device is also bound elsewhere: it parses, and a credential
/// place in the home has no second place.
#[test]
fn a_real_table_parses_and_finds_what_it_should() {
    let mounts = parse(include_bytes!("host.mountinfo")).expect("the real table parses");
    assert_eq!(mounts.len(), 19);
    assert!(mounts.iter().any(|m| m.kind == "nsfs" && m.root.is_none()));
    // A FUSE mount under the user's runtime directory is found where it is.
    let gvfs = mount_of(
        &mounts,
        &ids(&mounts),
        Path::new("/run/user/1000/gvfs/share"),
    )
    .expect("a mount");
    assert_eq!(
        (gvfs.kind.as_str(), gvfs.at.as_path()),
        ("fuse.gvfsd-fuse", Path::new("/run/user/1000/gvfs"))
    );
    assert!(mounts.iter().any(|m| m.kind == "autofs"));
    assert!(found(&mounts, "/home/user/.ssh").is_empty());
    // The bound container store is the home's own directory, there too.
    assert_eq!(
        found(&mounts, "/home/container-storage/docker/key"),
        [PathBuf::from("/var/lib/docker/key")]
    );
}

#[test]
fn a_table_it_cannot_read_fails_closed() {
    for bad in [
        &b"22 1 0:21 / / rw - ext4 /dev/sda1 rw\n23 x\n"[..],
        b"22 1 nodevice / / rw - ext4 /dev/sda1 rw\n",
        b"x 1 0:21 / / rw - ext4 /dev/sda1 rw\n",
        b"22 1 0:21 / rel rw - ext4 /dev/sda1 rw\n",
        b"22 1 0:21 / /a\\09 rw - ext4 /dev/sda1 rw\n",
        b"22 1 0:21 / / rw ext4 /dev/sda1 rw\n",
        b"22 1 0:21 / / rw -\n",
    ] {
        assert!(
            parse(bad).is_err_and(|e| e.to_string().contains("cannot be read")),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
    assert!(
        aliases(&[], &[PathBuf::from("/home/u")])
            .is_err_and(|e| e.to_string().contains("under no mount"))
    );
}

/// A mount point that is not UTF-8 is read as the bytes it is.
#[test]
fn a_mount_point_that_is_not_utf8_is_read() {
    let mounts = parse(
        b"22 1 0:21 / / rw - ext4 /dev/sda1 rw\n23 22 0:21 / /mnt/\xff rw - ext4 /dev/sda1 rw\n",
    )
    .expect("parses");
    use std::os::unix::ffi::OsStrExt;
    assert_eq!(mounts[1].at.as_os_str().as_bytes(), b"/mnt/\xff");
}

/// A denied place on NFS mounted more than once is refused: the table gives
/// every NFS mount `/` as its root, so it cannot say where the place is at
/// each. Mounted once, there is nothing to find. A FUSE filesystem shows the
/// directory a bind of part of it shows, and is followed as a local one is.
#[test]
fn a_network_filesystem_mounted_twice_is_refused() {
    let twice = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:50 / /home rw - nfs4 server:/export/home rw
3 1 0:50 / /mnt/home rw - nfs4 server:/export rw
";
    let mounts = parse(twice).expect("parses");
    assert!(
        aliases(&mounts, &[PathBuf::from("/home/u/.ssh")]).is_err_and(|e| e
            .to_string()
            .contains("nfs4 filesystem seen at /home, that is mounted again at /mnt/home"))
    );
    let once = parse(
        b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 0:50 / /home rw - nfs4 server:/export/home rw\n",
    )
    .expect("parses");
    assert!(found(&once, "/home/u/.ssh").is_empty());
}

/// The mount a path is in is the top of the stack at its mount point,
/// whatever order the table lists them in.
#[test]
fn the_mount_at_a_path_is_the_top_of_its_stack() {
    let stacked = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
5 4 8:2 / /home rw - ext4 /dev/sdb1 rw
4 1 8:1 /data /home rw - ext4 /dev/sda1 rw
6 1 8:2 / /mnt/b rw - ext4 /dev/sdb1 rw
";
    let mounts = parse(stacked).expect("parses");
    let home = mount_of(&mounts, &ids(&mounts), Path::new("/home/u")).expect("a mount");
    assert_eq!(home.id, 5);
    // Through the mount on top, sdb1, not the one it covers.
    assert_eq!(found(&mounts, "/home/u"), [PathBuf::from("/mnt/b/u")]);
}

/// A filesystem mounted inside a denied directory, on a device of its own,
/// is followed to its other mounts too.
#[test]
fn a_mount_inside_a_denied_directory_is_followed_to_its_other_mounts() {
    let inner = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:3 /keys /home/u/.ssh/keys rw - ext4 /dev/sdc1 rw
3 1 8:3 / /srv/c rw - ext4 /dev/sdc1 rw
";
    let mounts = parse(inner).expect("parses");
    assert_eq!(
        found(&mounts, "/home/u/.ssh"),
        [PathBuf::from("/srv/c/keys")]
    );
}

#[test]
fn a_fuse_filesystem_bound_in_part_is_followed() {
    absent("/home/u/.ssh");
    let fuse = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:60 / /mnt/fuse rw - fuse.sshfs host:/home rw
3 1 0:60 /u /home/u rw - fuse.sshfs host:/home rw
";
    let mounts = parse(fuse).expect("parses");
    assert_eq!(
        found(&mounts, "/home/u/.ssh"),
        [PathBuf::from("/mnt/fuse/u/.ssh")]
    );
}

/// A root mount that is its own parent, as an initramfs's is, is the root.
#[test]
fn a_root_that_is_its_own_parent_is_the_root() {
    absent("/home/u");
    let own = b"\
1 1 0:2 / / rw - rootfs rootfs rw
2 1 0:2 / /mnt/again rw - rootfs rootfs rw
";
    let mounts = parse(own).expect("parses");
    assert_eq!(
        found(&mounts, "/home/u"),
        [PathBuf::from("/mnt/again/home/u")]
    );
}

/// A mount on a mount the table does not list, other than the root, is a
/// table cut short, or not one at all: refused.
#[test]
fn a_mount_on_one_the_table_does_not_list_is_refused() {
    let cut = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
3 2 8:2 / /home rw - ext4 /dev/sdb1 rw
";
    assert!(
        parse(cut).is_err_and(|e| e.to_string().contains("on mount 2, which it does not list"))
    );
    assert!(
        parse(b"1 0 8:1 / /mnt rw - ext4 /dev/sda1 rw\n").is_err_and(|e| e
            .to_string()
            .contains("lists no mount at / that is mounted on nothing"))
    );
}

/// A FUSE mount whose root the kernel has lost, as when a lookup fails with
/// its server away, shows `<dir>//deleted`, not the directory it shows:
/// with its device mounted more than once, a denied place on it is refused,
/// whichever mount lost it.
#[test]
fn a_lost_fuse_root_is_refused() {
    for table in [
        &b"1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:60 / /mnt/fuse rw - fuse.sshfs host:/home rw
3 1 0:60 /u//deleted /home/u rw - fuse.sshfs host:/home rw
"[..],
        b"1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:60 /x//deleted /mnt/fuse rw - fuse.sshfs host:/home rw
3 1 0:60 /u /home/u rw - fuse.sshfs host:/home rw
",
    ] {
        let mounts = parse(table).expect("parses");
        assert!(
            aliases(&mounts, &[PathBuf::from("/home/u/.ssh")])
                .is_err_and(|e| e.to_string().contains("the kernel no longer knows")),
            "{:?}",
            String::from_utf8_lossy(table)
        );
    }
}

/// SMB mounted more than once is refused, as NFS is: its roots are not
/// trusted.
#[test]
fn smb_mounted_twice_is_refused() {
    let mounts = parse(
        b"1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:70 / /mnt/share rw - cifs //host/share rw
3 1 0:70 /u /home/u rw - cifs //host/share rw
",
    )
    .expect("parses");
    assert!(
        aliases(&mounts, &[PathBuf::from("/home/u/.ssh")])
            .is_err_and(|e| e.to_string().contains("cifs filesystem seen at /home/u"))
    );
}

/// A lost root on a filesystem that is not local is refused, whatever kind
/// it is, such as Ceph's when another client renames the bound directory.
#[test]
fn a_lost_root_on_any_remote_kind_is_refused() {
    for kind in ["ceph", "9p", "virtiofs", "afs"] {
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
             2 1 0:80 / /mnt/r rw - {kind} r rw\n\
             3 1 0:80 /home/u//deleted /home/u rw - {kind} r rw\n"
        );
        let mounts = parse(table.as_bytes()).expect("parses");
        assert!(
            aliases(&mounts, &[PathBuf::from("/home/u/.ssh")])
                .is_err_and(|e| e.to_string().contains("the kernel no longer knows")),
            "{kind}"
        );
    }
}

/// On a local filesystem a lost root is a file or directory replaced since
/// it was bound, as a container's resolv.conf is: the bind still shows the
/// old one, which is denied whole when the denied place is that file or a
/// directory above it.
#[test]
fn a_lost_root_on_a_local_kind_is_denied_whole() {
    let table = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:1 /data/creds//deleted /mnt/r rw - ext4 /dev/sda1 rw
";
    let mounts = parse(table).expect("parses");
    assert!(mounts[1].lost);
    assert_eq!(mounts[1].root.as_deref(), Some(Path::new("/data/creds")));
    for denied in ["/data/creds", "/data"] {
        assert_eq!(
            found(&mounts, denied),
            [PathBuf::from("/mnt/r")],
            "{denied}"
        );
    }
}

/// A 9p filesystem bound in part is followed as a local one is.
#[test]
fn a_9p_filesystem_bound_in_part_is_followed() {
    absent("/home/u/.ssh");
    let table = b"\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 0:90 / /mnt/host rw - 9p host rw
3 1 0:90 /home/u /home/u rw - 9p host rw
";
    let mounts = parse(table).expect("parses");
    assert_eq!(
        found(&mounts, "/home/u/.ssh"),
        [PathBuf::from("/mnt/host/home/u/.ssh")]
    );
}

/// Off a local filesystem a second place is checked against the place where
/// the place is there. A table made up with paths of its own holds only on
/// a host where they are not.
fn absent(path: &str) {
    assert!(
        std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "{path} is on this host, so a made-up table naming it is checked against it"
    );
}

/// Removed when dropped, whatever the test came to.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new directory of the test's own: its name is not one to guess, and it
/// is made only where nothing is.
fn scratch(tag: &str) -> Scratch {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..16 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let dir =
            std::env::temp_dir().join(format!("atomic-mounts-{tag}-{:016x}", hasher.finish()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Scratch(std::fs::canonicalize(&dir).expect("real")),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("scratch: {e}"),
        }
    }
    panic!("scratch: no new name");
}

/// A 9p filesystem mounted at `host`, with one more mount `also` of it,
/// whose root is `root`.
fn ninep(host: &Path, also: &Path, root: &str) -> Vec<Mount> {
    let table = format!(
        "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
         2 1 0:91 / {} rw - 9p host rw\n\
         3 1 0:91 {root} {} rw - 9p host rw\n",
        host.display(),
        also.display()
    );
    parse(table.as_bytes()).expect("parses")
}

/// Off a local filesystem, where the place is there, each second place
/// must show what its root says: one that is another directory, or is not
/// there, is refused; one that is, is denied. A mount of part of the place
/// is checked against that part, not the place.
#[test]
fn a_stale_remote_root_is_refused_where_the_place_is_there() {
    let dir = scratch("stale");
    let host = dir.0.join("host");
    let place = host.join("home/u/.ssh");
    std::fs::create_dir_all(place.join("keys")).expect("dir");
    std::fs::create_dir_all(dir.0.join("elsewhere/home/u/.ssh")).expect("dir");
    // A mount of the whole filesystem whose root says it shows what is at
    // `host`, but is another directory.
    let mounts = ninep(&host, &dir.0.join("elsewhere"), "/");
    assert!(
        aliases(&mounts, std::slice::from_ref(&place))
            .is_err_and(|e| e.to_string().contains("shows another file or directory"))
    );
    // One that is not there at all.
    let mounts = ninep(&host, &dir.0.join("nowhere"), "/");
    assert!(
        aliases(&mounts, std::slice::from_ref(&place))
            .is_err_and(|e| e.to_string().contains("is not there"))
    );
    // One that is a symbolic link to the place: a link is not the place,
    // and is not followed.
    std::fs::create_dir_all(dir.0.join("linked/home/u")).expect("dir");
    std::os::unix::fs::symlink(&place, dir.0.join("linked/home/u/.ssh")).expect("link");
    let mounts = ninep(&host, &dir.0.join("linked"), "/");
    assert!(
        aliases(&mounts, std::slice::from_ref(&place))
            .is_err_and(|e| e.to_string().contains("shows another file or directory"))
    );
    // A mount of part of the place over that part: checked against the
    // part it shows, which it is, so it passes.
    let keys = place.join("keys");
    let mounts = ninep(&host, &keys, "/home/u/.ssh/keys");
    let found = aliases(&mounts, std::slice::from_ref(&place)).expect("the same directory");
    assert_eq!(found, vec![keys]);
    // A mount of a part the place does not hold, perhaps from a stale root:
    // nothing to check it against, so it is denied as it is.
    let mounts = ninep(&host, &dir.0.join("elsewhere"), "/home/u/.ssh/gone");
    let found = aliases(&mounts, std::slice::from_ref(&place)).expect("denied unchecked");
    assert_eq!(found, vec![dir.0.join("elsewhere")]);
}

/// A place that cannot be looked at, other than for not being there, is
/// refused: that is how a stale root can show.
#[test]
fn a_place_that_cannot_be_looked_at_is_refused() {
    let dir = scratch("loop");
    let host = dir.0.join("host");
    std::fs::create_dir_all(&host).expect("dir");
    std::fs::create_dir_all(dir.0.join("also")).expect("dir");
    // A link to itself on the way to the place: looking fails with ELOOP
    // for anyone, root too.
    std::os::unix::fs::symlink("home", host.join("home")).expect("link");
    let place = host.join("home/u/.ssh");
    assert!(
        std::fs::symlink_metadata(&place).is_err_and(|e| e.raw_os_error() == Some(libc::ELOOP))
    );
    let mounts = ninep(&host, &dir.0.join("also"), "/");
    assert!(
        aliases(&mounts, std::slice::from_ref(&place))
            .is_err_and(|e| e.to_string().contains("cannot be looked at"))
    );
}

/// Another mount of the place's filesystem whose mount point is the place,
/// or a directory above it, shows the place there whatever its root says:
/// where the table yields that second place it passes, and where its root
/// says another directory, the table is stale and the list is refused. (A
/// mount point reached through a linked directory stands in here for a
/// mount of the same directory; the sandbox suite's `stale_roots` makes
/// real ones.)
#[test]
fn a_mount_of_a_directory_above_the_place_is_checked_whatever_its_root_says() {
    let dir = scratch("above");
    let home = dir.0.join("host/home");
    let place = home.join("u/.ssh");
    std::fs::create_dir_all(&place).expect("dir");
    std::os::unix::fs::symlink(&home, dir.0.join("link")).expect("link");
    let shown = dir.0.join("link/u");
    let table = |root: &Path| {
        format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} {} rw - ext4 /dev/sda1 rw\n",
            root.display(),
            shown.display()
        )
    };
    // Its root names the directory it is: the table yields it, and it passes.
    let mounts = parse(table(&home.join("u")).as_bytes()).expect("parses");
    assert_eq!(
        aliases(&mounts, std::slice::from_ref(&place)).expect("yielded"),
        [shown.join(".ssh")]
    );
    // Its root names another: refused.
    let mounts = parse(table(Path::new("/elsewhere")).as_bytes()).expect("parses");
    assert!(
        aliases(&mounts, std::slice::from_ref(&place)).is_err_and(|e| {
            let e = e.to_string();
            e.contains(&format!("the mount at {} shows", shown.display()))
                && e.contains("(/elsewhere) does not say")
        })
    );
}
