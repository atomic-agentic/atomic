// SPDX-License-Identifier: Apache-2.0

//! A mount table's root can go stale with nothing in it to say so: with a
//! directory of an overlay bound at a second path, renaming the lower
//! directory under it leaves the table naming the old one, until the new
//! name is first looked up through the overlay, while the bind shows the
//! place by its new name. atomic then cannot tell from the table
//! where that place is mounted again, and refuses the list rather than
//! leave it readable through the bind.
//!
//! The test runs itself again in a user and mount namespace of its own,
//! where it mounts a tmpfs, an overlay on it and a bind of one of the
//! overlay's directories; nothing outside sees the mounts. Needs
//! unprivileged user namespaces, so it runs with the `sandbox-tests`
//! feature; with the feature on, a namespace that cannot be made fails the
//! test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]

mod support;

use std::path::{Path, PathBuf};

/// Set in the run of this test inside its namespace, to its directory.
const CHILD: &str = "ATOMIC_STALE_ROOT_DIR";

/// The test's name, which the run inside the namespace runs alone.
const NAME: &str = "a_second_mount_whose_root_went_stale_refuses_the_list";

/// Run this test again in a user and mount namespace of its own, with a
/// tmpfs at `dir/t` holding an overlay at `t/merged` of `t/lower`, whose
/// `under` directory holds `.ssh/key`, and `t/merged/under` bound at
/// `t/second`.
fn run_with_an_overlay(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let t = dir.join("t");
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).expect("a path");
    let dirs: Vec<std::ffi::CString> = [
        "lower",
        "lower/under",
        "lower/under/.ssh",
        "upper",
        "work",
        "merged",
        "second",
    ]
    .iter()
    .map(|d| c(&t.join(d)))
    .collect();
    let key = c(&t.join("lower/under/.ssh/key"));
    let (tmpfs, merged, under, second) = (
        c(&t),
        c(&t.join("merged")),
        c(&t.join("merged/under")),
        c(&t.join("second")),
    );
    let options = std::ffi::CString::new(format!(
        "lowerdir={},upperdir={},workdir={}",
        t.join("lower").display(),
        t.join("upper").display(),
        t.join("work").display()
    ))
    .expect("options");
    // SAFETY: getuid and getgid cannot fail and touch no memory.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = format!("{uid} {uid} 1");
    let gid_map = format!("{gid} {gid} 1");
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test"));
    child
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir);
    // SAFETY: between fork and exec this calls only async-signal-safe
    // functions, on memory made before the fork.
    #[allow(unsafe_code)]
    unsafe {
        child.pre_exec(move || {
            let check = |r: libc::c_int| {
                if r < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(r)
                }
            };
            let write = |path: &[u8], text: &[u8], create: bool| -> std::io::Result<()> {
                let flags = if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_WRONLY
                };
                let fd = check(libc::open(path.as_ptr().cast(), flags, 0o600))?;
                let n = libc::write(fd, text.as_ptr().cast(), text.len());
                libc::close(fd);
                if n < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            write(b"/proc/self/setgroups\0", b"deny", false)?;
            write(b"/proc/self/uid_map\0", uid_map.as_bytes(), false)?;
            write(b"/proc/self/gid_map\0", gid_map.as_bytes(), false)?;
            let none = std::ptr::null();
            check(libc::mount(
                none,
                c"/".as_ptr(),
                none,
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            check(libc::mount(
                c"tmpfs".as_ptr(),
                tmpfs.as_ptr(),
                c"tmpfs".as_ptr(),
                0,
                std::ptr::null(),
            ))?;
            for d in &dirs {
                check(libc::mkdir(d.as_ptr(), 0o700))?;
            }
            write(key.as_bytes_with_nul(), b"the person's key", true)?;
            check(libc::mount(
                c"overlay".as_ptr(),
                merged.as_ptr(),
                c"overlay".as_ptr(),
                0,
                options.as_ptr().cast(),
            ))?;
            check(libc::mount(
                under.as_ptr(),
                second.as_ptr(),
                none,
                libc::MS_BIND,
                std::ptr::null(),
            ))?;
            Ok(())
        });
    }
    let out = child.output().expect("the test runs again");
    assert!(
        out.status.success(),
        "inside its namespace the test failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The harness's summary, whatever the test printed around its own line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(NAME) && stdout.contains("test result: ok. 1 passed"),
        "it did not run inside its namespace:\n{stdout}"
    );
}

/// The mount table's root for the mount at `at`.
fn root_of(at: &Path) -> String {
    let table = std::fs::read_to_string("/proc/self/mountinfo").expect("the mount table");
    let at = at.display().to_string();
    table
        .lines()
        .find_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields.get(4) == Some(&at.as_str())).then(|| fields[3].to_owned())
        })
        .unwrap_or_else(|| panic!("no mount at {at}:\n{table}"))
}

#[test]
fn a_second_mount_whose_root_went_stale_refuses_the_list() {
    support::require_sandbox();
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-stale-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("t")).expect("scratch");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        run_with_an_overlay(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let t = dir.join("t");
    let second = t.join("second");
    let place = t.join("merged/under/.ssh");
    // The positive control: the key is there at both, and the table names
    // the bind's root, so its second place is found and denied.
    for key in [place.join("key"), second.join(".ssh/key")] {
        assert_eq!(
            std::fs::read_to_string(&key).expect("the key"),
            "the person's key"
        );
    }
    assert_eq!(root_of(&second), "/under");
    let found = atomic_core::mounts::second_mounts(&[std::slice::from_ref(&place)]).expect("found");
    assert_eq!(found, [vec![second.join(".ssh")]]);
    // The lower directory renamed under the bind: the table keeps the old
    // root, with nothing to say it is stale, until the new name is first
    // looked up through the overlay, while the bind shows the place by its
    // new name. atomic reads the table, then looks: refused.
    std::fs::rename(t.join("lower/under"), t.join("lower/moved")).expect("rename");
    let place = t.join("merged/moved/.ssh");
    assert_eq!(root_of(&second), "/under");
    let refused = atomic_core::mounts::second_mounts(&[std::slice::from_ref(&place)])
        .expect_err("refused")
        .to_string();
    assert!(
        refused.contains(&format!("the mount at {} shows", second.display()))
            && refused.contains("(/under) does not say"),
        "{refused}"
    );
    // What was at stake: the key, by the place's new name and through the
    // bind.
    for key in [place.join("key"), second.join(".ssh/key")] {
        assert_eq!(
            std::fs::read_to_string(&key).expect("the key"),
            "the person's key"
        );
    }
}
