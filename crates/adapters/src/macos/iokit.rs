//! Apple Silicon GPU facts straight from IOKit (no subprocess).
//!
//! Adapters must never spawn processes — the supervisor owns every child —
//! so the `IOAccelerator` properties are read through IOKit instead of the
//! `ioreg` CLI with the same property names: `Device Utilization %`,
//! `Renderer/Tiler Utilization %`, `In use/Alloc system memory`, `model`
//! and `gpu-core-count`. Unified
//! memory totals come from `hw.memsize` (the GPU shares system memory).
//! All Mach and CoreFoundation calls stay inside [`read_gpu`]; every block
//! states why it is sound, and every handle is released on every path.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};

use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
};
use core_foundation_sys::number::{
    CFNumberGetTypeID, CFNumberGetValue, CFNumberRef, kCFNumberSInt64Type,
};
use core_foundation_sys::string::{
    CFStringCreateWithCString, CFStringGetCString, CFStringGetTypeID, CFStringRef,
    kCFStringEncodingUTF8,
};
use io_kit_sys::ret::kIOReturnSuccess;
use io_kit_sys::types::io_iterator_t;
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching, kIOMasterPortDefault,
};

/// One Apple GPU as IOKit describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AppleGpu {
    pub model: String,
    pub core_count: u64,
    pub device_util_pct: u64,
    pub renderer_util_pct: u64,
    pub tiler_util_pct: u64,
    pub mem_used_bytes: u64,
    pub mem_alloc_bytes: u64,
}

/// Reads the first `IOAccelerator` service. Fails with a bounded reason
/// code when the service or its properties are absent.
pub(crate) fn read_gpu() -> Result<AppleGpu, &'static str> {
    // SAFETY: the whole IOKit transaction runs here; each inner block
    // justifies its own pointer and lifetime handling.
    unsafe { read_properties() }
}

/// First `IOAccelerator` service's properties, extracted to plain scalars.
///
/// # Safety
///
/// Runs on any thread; every CoreFoundation object is created and released
/// inside, and no pointer outlives the dictionary it was read from.
unsafe fn read_properties() -> Result<AppleGpu, &'static str> {
    const SERVICE_ABSENT: &str = "APPLE_GPU_SERVICE_ABSENT";
    const PROPERTIES_ABSENT: &str = "APPLE_GPU_PROPERTIES_ABSENT";
    let name = CString::new("IOAccelerator").map_err(|_| SERVICE_ABSENT)?;
    // SAFETY: `name` is a valid NUL-terminated class name; the returned
    // matching dictionary is owned by the caller.
    let matching = unsafe { IOServiceMatching(name.as_ptr()) };
    if matching.is_null() {
        return Err(SERVICE_ABSENT);
    }
    let mut iterator: io_iterator_t = 0;
    // SAFETY: the master port is a task-owned send right; `iterator` is a
    // live out-parameter. On success the matching dictionary is consumed
    // by the call, so it must not be released on this path.
    let status =
        unsafe { IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iterator) };
    if status != kIOReturnSuccess {
        // SAFETY: the failed call did not consume the dictionary.
        unsafe { CFRelease(matching as CFTypeRef) };
        return Err(SERVICE_ABSENT);
    }
    // SAFETY: a valid iterator yields owned service handles (zero at the
    // end); the iterator itself is always released.
    let service = unsafe { IOIteratorNext(iterator) };
    unsafe { IOObjectRelease(iterator) };
    if service == 0 {
        return Err(SERVICE_ABSENT);
    }
    let mut properties: core_foundation_sys::dictionary::CFMutableDictionaryRef =
        std::ptr::null_mut();
    // SAFETY: `service` is a live handle; `properties` is a live
    // out-parameter for the created dictionary the caller then owns.
    let status = unsafe {
        IORegistryEntryCreateCFProperties(service, &mut properties, kCFAllocatorDefault, 0)
    };
    // SAFETY: the service handle was consumed for this read.
    unsafe { IOObjectRelease(service) };
    if status != kIOReturnSuccess || properties.is_null() {
        return Err(PROPERTIES_ABSENT);
    }
    // SAFETY: `properties` is a live dictionary for the extraction below,
    // released exactly once afterwards on every path.
    let gpu = unsafe { extract(properties) };
    unsafe { CFRelease(properties as CFTypeRef) };
    gpu.ok_or(PROPERTIES_ABSENT)
}

/// Plain scalars out of a live `IOAccelerator` property dictionary.
///
/// # Safety
///
/// `properties` must be a live `CFDictionary`; values are only borrowed for
/// immediate conversion, and every temporary key string is released.
unsafe fn extract(
    properties: core_foundation_sys::dictionary::CFMutableDictionaryRef,
) -> Option<AppleGpu> {
    // SAFETY: reading through the live dictionary pointer with temporary
    // NUL-terminated keys; each key is released after its lookup.
    unsafe {
        let stats = dict_value(properties as CFDictionaryRef, "PerformanceStatistics")?;
        if CFGetTypeID(stats) != CFDictionaryGetTypeID() {
            return None;
        }
        let stats = stats as CFDictionaryRef;
        let mut model = [0u8; 256];
        let model_len = dict_string(properties as CFDictionaryRef, "model", &mut model)?;
        Some(AppleGpu {
            model: String::from_utf8_lossy(&model[..model_len])
                .trim()
                .to_owned(),
            core_count: dict_uint(properties as CFDictionaryRef, "gpu-core-count").unwrap_or(0),
            device_util_pct: dict_percent(stats, "Device Utilization %"),
            renderer_util_pct: dict_percent(stats, "Renderer Utilization %"),
            tiler_util_pct: dict_percent(stats, "Tiler Utilization %"),
            mem_used_bytes: dict_uint(stats, "In use system memory").unwrap_or(0),
            mem_alloc_bytes: dict_uint(stats, "Alloc system memory").unwrap_or(0),
        })
    }
}

/// Raw value of one property, or `None` when missing.
unsafe fn dict_value(dict: CFDictionaryRef, key: &str) -> Option<CFTypeRef> {
    let ckey = CString::new(key).ok()?;
    // SAFETY: `ckey` is NUL-terminated; the created key string is owned and
    // released after the lookup. A missing key yields NULL, never a trap.
    let key_ref = unsafe {
        CFStringCreateWithCString(kCFAllocatorDefault, ckey.as_ptr(), kCFStringEncodingUTF8)
    };
    if key_ref.is_null() {
        return None;
    }
    let value = unsafe { CFDictionaryGetValue(dict, key_ref as *const std::ffi::c_void) };
    // SAFETY: the temporary key string was created above.
    unsafe { CFRelease(key_ref as CFTypeRef) };
    if value.is_null() { None } else { Some(value) }
}

/// Unsigned integer property; floats and out-of-range values read as absent.
unsafe fn dict_uint(dict: CFDictionaryRef, key: &str) -> Option<u64> {
    // SAFETY: `value` is non-NULL with a number type id (checked below),
    // so reading it as an `i64` is sound.
    unsafe {
        let value = dict_value(dict, key)?;
        if CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut out: i64 = 0;
        if !CFNumberGetValue(
            value as CFNumberRef,
            kCFNumberSInt64Type,
            (&mut out as *mut i64).cast::<std::ffi::c_void>(),
        ) {
            return None;
        }
        u64::try_from(out).ok()
    }
}

/// Utilization percent clamped to 0–100 (a missing property reads as idle,
/// never as an error: the sampler publishes what the counter offers).
unsafe fn dict_percent(dict: CFDictionaryRef, key: &str) -> u64 {
    // SAFETY: pure conversion of the looked-up value.
    unsafe { dict_uint(dict, key).unwrap_or(0).min(100) }
}

/// UTF-8 string property into `buffer`; returns the byte length without the
/// NUL terminator.
unsafe fn dict_string(dict: CFDictionaryRef, key: &str, buffer: &mut [u8]) -> Option<usize> {
    // SAFETY: `value` is non-NULL with a string type id (checked below);
    // `buffer` is writable for `capacity` bytes and NUL-terminated by the
    // call on success.
    unsafe {
        let value = dict_value(dict, key)?;
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        let capacity = core_foundation_sys::base::CFIndex::try_from(buffer.len()).unwrap_or(0);
        if CFStringGetCString(
            value as CFStringRef,
            buffer.as_mut_ptr().cast::<std::ffi::c_char>(),
            capacity,
            kCFStringEncodingUTF8,
        ) == 0
        {
            return None;
        }
        Some(
            CStr::from_ptr(buffer.as_ptr() as *const std::ffi::c_char)
                .to_bytes()
                .len(),
        )
    }
}
