//! Safe wrappers over macOS `sysctlbyname(3)`.
//!
//! This module mirrors the runtime's sysctl helper: the adapters crate must
//! not depend on the runtime (dependency boundaries, D-79), so the tiny
//! wrapper lives here too. `unsafe` is contained in these two functions;
//! every block states why it is sound.
#![allow(unsafe_code)]

use std::ffi::CString;

/// Reads a `sysctl` OID as a string (`machdep.cpu.brand_string`,
/// `hw.model`, `kern.hostname`, ...). Returns `None` when the OID is
/// missing, unreadable or empty after trimming NULs and whitespace.
pub(crate) fn string(name: &str) -> Option<String> {
    let bytes = bytes_of(name)?;
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim_matches('\0').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Reads a `sysctl` OID holding one native integer (`hw.memsize`,
/// `hw.ncpu`, `hw.pagesize`, ...). Both 32- and 64-bit values are accepted.
pub(crate) fn u64_value(name: &str) -> Option<u64> {
    let bytes = bytes_of(name)?;
    match bytes.len() {
        4 => Some(u32::from_ne_bytes(bytes[..4].try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(bytes[..8].try_into().ok()?)),
        _ => None,
    }
}

/// Raw bytes of a `sysctl` OID via the classic NULL-probe + fetch pattern.
fn bytes_of(name: &str) -> Option<Vec<u8>> {
    let oid = CString::new(name).ok()?;
    let mut size: libc::size_t = 0;
    // SAFETY: `oid` is a valid NUL-terminated C string that outlives the
    // call; `oldp` is NULL with a live `oldlenp`, the documented size-probe
    // pattern, and no write end is passed.
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
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: `buffer` holds `size` initialized bytes for the kernel to fill
    // (it updates `size` to the actual count); same live `oid`, NULL write
    // end as required for a read.
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
        return None;
    }
    buffer.truncate(size);
    Some(buffer)
}
