// SPDX-License-Identifier: Apache-2.0

//! Numeric context for a rejected child memory limit on macOS.

#![allow(unsafe_code)]

/// Darwin's process totals explain startup failures but cannot establish a
/// memory limit. The reported virtual size is adjusted by the OS.
pub(crate) fn rejected_data_limit(requested: u64, error: &std::io::Error) -> String {
    let mut message = error.to_string();
    if error.raw_os_error() != Some(libc::EINVAL) {
        return message;
    }

    use std::fmt::Write;
    let _ = write!(message, " (requested data limit {requested} bytes");
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is writable storage for the system call's result.
    if unsafe { libc::getrlimit(libc::RLIMIT_DATA, &mut limit) } == 0 {
        let _ = write!(
            message,
            "; inherited soft limit {}, hard limit {}",
            limit.rlim_cur, limit.rlim_max
        );
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` has the size and alignment expected by PROC_PIDTASKINFO.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written == size {
        // SAFETY: The kernel reported writing the full proc_taskinfo.
        let info = unsafe { info.assume_init() };
        let _ = write!(
            message,
            "; adjusted virtual size {} bytes, resident size {} bytes",
            info.pti_virtual_size, info.pti_resident_size
        );
    }
    #[cfg(target_arch = "aarch64")]
    if let Some((bytes, protection, maximum)) = gpu_reservation() {
        let _ = write!(
            message,
            "; GPU address-space reservation {bytes} bytes, protection {protection:#x}, maximum protection {maximum:#x}"
        );
    }
    message.push(')');
    message
}

// XNU arm64 reserves an inaccessible region for GPU address space. Probe
// for it separately from the adjusted totals. These values are diagnostics.
#[cfg(target_arch = "aarch64")]
fn gpu_reservation() -> Option<(u64, libc::c_int, libc::c_int)> {
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_vm_region_recurse(
            task: libc::mach_port_t,
            address: *mut u64,
            size: *mut u64,
            nesting_depth: *mut libc::c_uint,
            info: *mut libc::c_int,
            info_count: *mut libc::c_uint,
        ) -> libc::c_int;
    }

    const GPU_RESERVATION_START: u64 = 0x0010_0000_0000;
    // The SDK packs vm_region_submap_short_info_64 to four-byte alignment.
    // Its first two words are protection and max_protection.
    let mut info = [0; 12];
    let mut count = info.len() as libc::c_uint;
    let mut address = GPU_RESERVATION_START;
    let mut size = 0;
    let mut depth = 0;
    // SAFETY: The task is this process. The outputs have the size and
    // alignment required by the Mach call and live until it returns.
    let result = unsafe {
        mach_vm_region_recurse(
            mach_task_self_,
            &mut address,
            &mut size,
            &mut depth,
            info.as_mut_ptr(),
            &mut count,
        )
    };
    (result == 0 && address == GPU_RESERVATION_START && count as usize == info.len())
        .then_some((size, info[0], info[1]))
}
