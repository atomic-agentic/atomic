// SPDX-License-Identifier: Apache-2.0

//! Make a private-profile worktree usable by PowerShell's filesystem provider.
//! Codex grants the worktree itself to the restricted token. PowerShell also
//! resolves every parent of its initial location, while cmd.exe can operate on
//! the inherited directory handle without resolving those parents. Grant only
//! traverse and attribute access on the parents; never grant listing, data read,
//! write, or inheritable access to the rest of the operator's profile.

use anyhow::{Context, Result, bail};
use codex_windows_sandbox::{
    LocalSid, OFFLINE_USERNAME, ONLINE_USERNAME, ensure_allow_mask_aces_with_inheritance,
    resolve_sid, workspace_cap_sid_for_cwd,
};
use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows_sys::Win32::Security::NO_INHERITANCE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_TRAVERSE,
};

pub(crate) fn allow_private_profile_cwd_ancestors(cwd: &Path, codex_home: &Path) -> Result<()> {
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        bail!("USERPROFILE is required for the Windows sandbox worktree");
    };
    let profile =
        dunce::canonicalize(Path::new(&profile)).context("resolve Windows operator profile")?;
    let cwd = dunce::canonicalize(cwd).context("resolve Windows sandbox worktree")?;
    if !cwd.starts_with(&profile) {
        return Ok(());
    }

    // Codex's elevated setup has provisioned these principals and the
    // workspace capability before this function runs. Both account and
    // restricted capability ACL checks must succeed for the shell token.
    let mut offline = resolve_sid(OFFLINE_USERNAME).context("resolve offline sandbox account")?;
    let mut online = resolve_sid(ONLINE_USERNAME).context("resolve online sandbox account")?;
    let workspace = LocalSid::from_string(
        &workspace_cap_sid_for_cwd(codex_home, &cwd)
            .context("resolve Windows sandbox workspace capability")?,
    )?;
    // The buffers own valid Windows SIDs through every ACL call below. Codex's
    // ACL API accepts mutable opaque pointers but only reads these SID bytes.
    let sids = [
        offline.as_mut_ptr().cast::<c_void>(),
        online.as_mut_ptr().cast::<c_void>(),
        workspace.as_ptr(),
    ];

    for ancestor in cwd
        .ancestors()
        .skip(1)
        .take_while(|parent| parent.starts_with(&profile))
    {
        let metadata = std::fs::symlink_metadata(ancestor)
            .with_context(|| format!("inspect Windows sandbox ancestor {}", ancestor.display()))?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "Windows sandbox ancestor is not an ordinary directory: {}",
                ancestor.display()
            );
        }
        // SAFETY: Codex resolved the account SIDs into owned buffers and owns
        // both capability SIDs until this call returns. Every ancestor exists
        // and is an ordinary directory. The helper checks existing ACEs and
        // preserves deny entries; NO_INHERITANCE confines the grant to this
        // directory, so unrelated private children do not gain read access.
        #[allow(unsafe_code)]
        unsafe {
            ensure_allow_mask_aces_with_inheritance(
                ancestor,
                &sids,
                FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
                NO_INHERITANCE,
            )
        }
        .with_context(|| format!("allow sandbox traversal of {}", ancestor.display()))?;
    }
    Ok(())
}
