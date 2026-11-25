// SPDX-License-Identifier: Apache-2.0
//! Keep a compiler or helper and its descendants in a bounded job owned by the
//! supervising process. A worker receives no project input before assignment.
#![allow(unsafe_code)]
use std::io::{self, Read as _, Write as _};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command};
use std::ptr;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};

const FLAGS: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
    | JOB_OBJECT_LIMIT_PROCESS_TIME;

fn ticks(seconds: u64) -> io::Result<i64> {
    seconds
        .checked_mul(10_000_000)
        .and_then(|v| i64::try_from(v).ok())
        .ok_or_else(|| io::Error::other("worker CPU limit is too large"))
}

pub(crate) fn spawn(
    command: &mut Command,
    memory: u64,
    seconds: u64,
) -> io::Result<(Child, OwnedHandle)> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = FLAGS;
    limits.BasicLimitInformation.PerProcessUserTimeLimit = ticks(seconds)?;
    limits.ProcessMemoryLimit = usize::try_from(memory).map_err(io::Error::other)?;
    // SAFETY: an unnamed non-inheritable job. On success we own its sole handle.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: the buffer and size correspond to the requested information class.
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Detach from the operator console. The protocol uses only the three pipes.
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let mut child = command.creation_flags(CREATE_NO_WINDOW).spawn()?;
    // SAFETY: both handles are live until after the call.
    let assigned = unsafe { AssignProcessToJobObject(raw, child.as_raw_handle()) };
    let ready = if assigned == 0 {
        Err(io::Error::last_os_error())
    } else {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("worker has no input pipe"))
            .and_then(|stdin| stdin.write_all(b"J"))
    };
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

pub(crate) fn wait_for_job() -> io::Result<()> {
    let mut ready = [0];
    io::stdin().read_exact(&mut ready)?;
    if ready != *b"J" {
        return Err(io::Error::other("worker job handshake failed"));
    }
    Ok(())
}

/// Check the limits before entering the compiler or evaluating helper code,
/// including when somebody invokes a private worker mode directly.
pub(crate) fn verify(memory: u64, seconds: u64) -> io::Result<()> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: NULL selects this process's current job, and limits is writable.
    if unsafe {
        QueryInformationJobObject(
            ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of_val(&limits) as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let basic = limits.BasicLimitInformation;
    if basic.LimitFlags & FLAGS != FLAGS
        || limits.ProcessMemoryLimit as u64 > memory
        || basic.PerProcessUserTimeLimit > ticks(seconds)?
    {
        return Err(io::Error::other(
            "worker job does not enforce the requested limits",
        ));
    }
    Ok(())
}
