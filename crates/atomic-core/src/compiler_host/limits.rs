// SPDX-License-Identifier: Apache-2.0

//! The operating system's limits on a compiler child, which bound what the
//! V8 heap limit does not: WebAssembly memories and ArrayBuffer contents,
//! which V8 allocates outside its heap, and CPU time. A child that V8
//! aborts leaves no core file, which would hold atom sources.
//!
//! macOS has no memory limit a child can set on itself, so there the parent
//! that waits on the child holds it (see `child_memory`).

#![allow(unsafe_code)]

/// Writable memory a compile may hold outside its V8 heap: V8's code and
/// platform threads, the binary's own data, the worker's source, the Go
/// parser's WebAssembly memory and ArrayBuffer contents. Checking the whole
/// example catalog, TypeScript checks included, needs between 768 and 832 MiB
/// of writable memory in all, the heap it uses counted; with the 1 GiB heap
/// limit the bound is 2.5 GiB.
pub(crate) const OUTSIDE_HEAP: u64 = 512 * 1024 * 1024;

/// CPU seconds a compile may use, over all of its threads. The parent stops
/// it after 30 seconds of wall-clock time in any case.
pub(crate) const CPU_SECONDS: u64 = 32;

/// The writable memory a child with heap limit `heap` may hold: the heap may
/// grow to twice its limit while a compile unwinds.
pub(crate) fn memory_limit(heap: usize) -> u64 {
    OUTSIDE_HEAP + 2 * heap as u64
}

/// Limits this process's CPU time and its core files and, except on macOS,
/// its writable memory to `memory` bytes.
pub(crate) fn limit_self(memory: u64) -> Result<(), String> {
    // macOS holds a data limit against the size of the whole address map and
    // refuses one below it, which any limit on memory is before the child
    // has done anything.
    #[cfg(target_os = "macos")]
    let _ = memory;
    // At the CPU limit the kernel sends SIGXCPU, which the parent reports as
    // that limit; a second later, at the hard limit, SIGKILL.
    for (name, resource, value, hard) in [
        #[cfg(not(target_os = "macos"))]
        ("memory", libc::RLIMIT_DATA, memory, 0),
        ("CPU time", libc::RLIMIT_CPU, CPU_SECONDS, 1),
        ("core files", libc::RLIMIT_CORE, 0, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value.saturating_add(hard) as libc::rlim_t,
        };
        // SAFETY: `limit` is a valid rlimit that outlives the call; lowering
        // this process's own limits has no other effect on memory safety.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(format!(
                "cannot limit the Astro compiler's {name}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}
