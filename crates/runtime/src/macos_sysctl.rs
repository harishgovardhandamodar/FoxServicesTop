//! Safe wrappers over the macOS `sysctlbyname(3)` interface.
//!
//! This is the only module of the crate (besides `pidfd` on Linux) allowed
//! to use `unsafe`: every block states why it is sound. Callers only see
//! safe functions returning owned data.
//!
//! Link note: `libc::sysctlbyname` resolves against libSystem, which every
//! macOS process links. No extra link flags are needed.
#![allow(unsafe_code)]

use std::ffi::CString;

use fox_services_top_core::errors::InfraError;

/// Reads a `sysctl` OID as a string (`kern.hostname`, `kern.osrelease`,
/// `kern.uuid`, ...). String, integer and opaque values are all returned as
/// their raw bytes interpreted lossily; callers trim and validate.
pub(crate) fn sysctl_string(name: &str) -> Result<String, InfraError> {
    let bytes = sysctl_bytes(name)?;
    // Opaque values (e.g. `kern.boottime`'s `struct timeval`) are not UTF-8;
    // only the string-OID callers use this helper, and lossy conversion
    // never fails while keeping the human-readable prefix intact.
    Ok(String::from_utf8_lossy(&bytes)
        .trim_matches('\0')
        .to_owned())
}

/// Reads a `sysctl` OID as raw bytes (used for `kern.boottime`).
pub(crate) fn sysctl_bytes(name: &str) -> Result<Vec<u8>, InfraError> {
    let oid = CString::new(name)
        .map_err(|_| InfraError::Unavailable(format!("sysctl name rejected: {name}")))?;
    // Probe the size first: a NULL buffer with a valid length pointer makes
    // the kernel return the required size without copying anything out.
    let mut size: libc::size_t = 0;
    // SAFETY: `oid` is a valid NUL-terminated C string that lives for the
    // call; `newp` is NULL (no write) and `newlen` is 0 as required for a
    // size probe; `oldlenp` points to a live stack slot.
    let probed = unsafe {
        libc::sysctlbyname(
            oid.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if probed != 0 || size == 0 || size > 1 << 20 {
        return Err(InfraError::Unavailable(format!(
            "sysctl size probe failed: {name}"
        )));
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: `buffer` has `size` initialized bytes and lives for the call;
    // the kernel writes at most `size` bytes (it updates `size` to the
    // actual count on success). Same valid `oid` and NULL write end.
    let fetched = unsafe {
        libc::sysctlbyname(
            oid.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if fetched != 0 {
        return Err(InfraError::Unavailable(format!(
            "sysctl read failed: {name}"
        )));
    }
    buffer.truncate(size);
    Ok(buffer)
}
