//! Boot clock and host identity.
//!
//! Linux uses `CLOCK_BOOTTIME` (keeps counting across suspend) and the
//! `/proc/sys/kernel/random/boot_id` pseudo file. macOS has neither, so the
//! Apple Silicon build uses `CLOCK_MONOTONIC` and the `kern.boottime`
//! sysctl as the boot identity; the host identity falls back to the stable
//! `kern.uuid` sysctl. Ages and deadlines stay meaningful within one boot
//! on both systems; only the Linux build survives a suspend without a jump.

use std::fs::File;
use std::io::Read;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fox_services_top_core::clock::{NANOS_PER_SECOND, TimeProvider};
use fox_services_top_core::digest::sha256_hex;
use fox_services_top_core::errors::InfraError;
use fox_services_top_core::identity::{BootId, HostId, PidNamespace};
use fox_services_top_core::text::sanitize_display;
use nix::time::{ClockId, clock_gettime};

#[cfg(target_os = "macos")]
use crate::macos_sysctl::{sysctl_bytes, sysctl_string};

#[cfg(not(target_os = "macos"))]
const BOOT_ID_PATH: &str = "proc/sys/kernel/random/boot_id";
const MACHINE_ID_PATHS: [&str; 2] = ["etc/machine-id", "var/lib/dbus/machine-id"];
const HOSTNAME_PATH: &str = "proc/sys/kernel/hostname";
const OSRELEASE_PATH: &str = "proc/sys/kernel/osrelease";
/// Pseudo files read here are tiny; the cap protects against surprises.
const MAX_IDENTITY_FILE_BYTES: u64 = 4096;
/// Hex digits of the machine-id digest kept in the host id.
const HOST_DIGEST_HEX_DIGITS: usize = 16;
/// Display bound for host names and kernel releases.
const MAX_HOST_TEXT_BYTES: usize = 256;

/// `TimeProvider` backed by `clock_gettime(CLOCK_BOOTTIME)` and `SystemTime`.
#[derive(Debug)]
pub struct BootClock {
    boot_id: BootId,
    last_boot_ns: AtomicU64,
}

impl BootClock {
    /// Reads the kernel boot identity and proves the boot clock is readable.
    pub fn new() -> Result<Self, InfraError> {
        Self::with_root(Path::new("/"))
    }

    /// Same as [`BootClock::new`] with `/proc` resolved under `root` (tests).
    ///
    /// On macOS there is no `/proc`, so the boot id comes from the
    /// `kern.boottime` sysctl and `root` is ignored.
    pub fn with_root(root: &Path) -> Result<Self, InfraError> {
        let boot_id = boot_identity(root)?;
        let first = read_boottime_ns()?;
        Ok(Self {
            boot_id,
            last_boot_ns: AtomicU64::new(first),
        })
    }
}

/// Boot identity of this boot: `/proc/sys/kernel/random/boot_id` on Linux,
/// the `kern.boottime` sysctl (`macos-boot-<sec>-<usec>`) on macOS.
#[cfg(target_os = "macos")]
fn boot_identity(_root: &Path) -> Result<BootId, InfraError> {
    // `kern.boottime` is a `struct timeval`; the raw bytes are
    // `{ sec: i64 LE, usec: i32 LE }` on Apple Silicon (little-endian).
    let bytes = sysctl_bytes("kern.boottime")?;
    if bytes.len() < 12 {
        return Err(InfraError::Unavailable(
            "kern.boottime returned too few bytes".into(),
        ));
    }
    let sec = i64::from_le_bytes(
        bytes[0..8]
            .try_into()
            .map_err(|_| InfraError::Unavailable("kern.boottime timestamp unreadable".into()))?,
    );
    let usec = i32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| InfraError::Unavailable("kern.boottime timestamp unreadable".into()))?,
    );
    if sec <= 0 {
        return Err(InfraError::Unavailable(
            "kern.boottime reports no boot time".into(),
        ));
    }
    BootId::new(format!("macos-boot-{sec}-{usec}"))
        .map_err(|error| InfraError::Unavailable(format!("invalid boot_id: {error}")))
}

/// Boot identity of this boot from `/proc` (Linux).
#[cfg(not(target_os = "macos"))]
fn boot_identity(root: &Path) -> Result<BootId, InfraError> {
    let text = read_identity_file(&root.join(BOOT_ID_PATH), "clock.boot_id")?;
    BootId::new(text.trim())
        .map_err(|error| InfraError::Unavailable(format!("invalid boot_id: {error}")))
}

impl TimeProvider for BootClock {
    fn boot_id(&self) -> BootId {
        self.boot_id.clone()
    }

    fn now_boot_ns(&self) -> u64 {
        match read_boottime_ns() {
            Ok(now) => {
                let previous = self.last_boot_ns.fetch_max(now, Ordering::AcqRel);
                previous.max(now)
            }
            // Readability was proven at construction; a transient failure must
            // never make time run backwards, so the last observation repeats.
            Err(_) => self.last_boot_ns.load(Ordering::Acquire),
        }
    }

    fn now_utc_ns(&self) -> i128 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => i128::try_from(elapsed.as_nanos()).unwrap_or(i128::MAX),
            Err(before) => i128::try_from(before.duration().as_nanos())
                .map(|nanos| -nanos)
                .unwrap_or(i128::MIN),
        }
    }
}

fn read_boottime_ns() -> Result<u64, InfraError> {
    // `CLOCK_BOOTTIME` keeps counting across suspend; macOS has no equivalent
    // (its `CLOCK_MONOTONIC` stops in deep sleep), so Apple Silicon accepts
    // the suspend jump while Linux keeps the suspend-aware clock.
    #[cfg(target_os = "macos")]
    let clock = ClockId::CLOCK_MONOTONIC;
    #[cfg(not(target_os = "macos"))]
    let clock = ClockId::CLOCK_BOOTTIME;
    #[cfg(target_os = "macos")]
    let label = "CLOCK_MONOTONIC";
    #[cfg(not(target_os = "macos"))]
    let label = "CLOCK_BOOTTIME";
    let now = clock_gettime(clock)
        .map_err(|errno| InfraError::Unavailable(format!("{label} unavailable: {errno}")))?;
    let seconds = u64::try_from(now.tv_sec())
        .map_err(|_| InfraError::Unavailable(format!("{label} returned negative seconds")))?;
    let nanos = u64::try_from(now.tv_nsec())
        .map_err(|_| InfraError::Unavailable(format!("{label} returned negative nanoseconds")))?;
    seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|whole| whole.checked_add(nanos))
        .ok_or_else(|| InfraError::Unavailable(format!("{label} overflows u64 ns")))
}

/// Stable host identity: `host-` + the first 16 hex digits of SHA-256 over
/// `/etc/machine-id`, falling back to `/var/lib/dbus/machine-id`, then (on
/// macOS) the stable `kern.uuid` sysctl, and finally the kernel host name.
/// The raw value never leaves this function.
pub fn host_identity(root: &Path) -> Result<HostId, InfraError> {
    let material = MACHINE_ID_PATHS
        .iter()
        .find_map(|relative| non_empty_identity(&root.join(relative)))
        .or_else(macos_machine_identity)
        .or_else(|| non_empty_identity(&root.join(HOSTNAME_PATH)))
        .ok_or_else(|| {
            InfraError::Unavailable("no machine-id, dbus machine-id or hostname available".into())
        })?;
    let digest = sha256_hex(material.as_bytes());
    let short = digest.get(..HOST_DIGEST_HEX_DIGITS).unwrap_or(&digest);
    HostId::new(format!("host-{short}"))
        .map_err(|error| InfraError::Unavailable(format!("host id rejected: {error}")))
}

/// Stable machine identity on macOS: the `kern.uuid` sysctl persists across
/// boots (unlike the boot time) and needs no privileges to read.
#[cfg(target_os = "macos")]
fn macos_machine_identity() -> Option<String> {
    sysctl_string("kern.uuid")
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// No machine-specific identity between the Linux files on other systems.
#[cfg(not(target_os = "macos"))]
fn macos_machine_identity() -> Option<String> {
    None
}

fn non_empty_identity(path: &Path) -> Option<String> {
    let text = read_identity_file(path, "clock.host_identity").ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Kernel host name (`/proc/sys/kernel/hostname`; the `kern.hostname`
/// sysctl where there is no `/proc`, i.e. on macOS), sanitized for display.
pub fn host_name() -> Result<String, InfraError> {
    host_name_at(Path::new("/"))
}

/// [`host_name`] with `/proc` resolved under `root`.
pub fn host_name_at(root: &Path) -> Result<String, InfraError> {
    if let Ok(name) = read_display_text(&root.join(HOSTNAME_PATH), "clock.hostname") {
        return Ok(name);
    }
    #[cfg(target_os = "macos")]
    if root == Path::new("/") {
        return macos_display_text("kern.hostname", "clock.hostname");
    }
    read_display_text(&root.join(HOSTNAME_PATH), "clock.hostname")
}

/// Kernel release (`/proc/sys/kernel/osrelease`; the `kern.osrelease`
/// sysctl on macOS, e.g. `27.0.0` for macOS 26 Tahoe), sanitized for display.
pub fn kernel_release() -> Result<String, InfraError> {
    kernel_release_at(Path::new("/"))
}

/// [`kernel_release`] with `/proc` resolved under `root`.
pub fn kernel_release_at(root: &Path) -> Result<String, InfraError> {
    if let Ok(release) = read_display_text(&root.join(OSRELEASE_PATH), "clock.osrelease") {
        return Ok(release);
    }
    #[cfg(target_os = "macos")]
    if root == Path::new("/") {
        return macos_display_text("kern.osrelease", "clock.osrelease");
    }
    read_display_text(&root.join(OSRELEASE_PATH), "clock.osrelease")
}

/// Display text of a macOS sysctl OID, sanitized like the `/proc` values.
#[cfg(target_os = "macos")]
fn macos_display_text(oid: &str, operation: &'static str) -> Result<String, InfraError> {
    let text = sysctl_string(oid)
        .map_err(|_| InfraError::Unavailable(format!("{operation}: sysctl {oid} unreadable")))?;
    let clean = sanitize_display(text.trim(), MAX_HOST_TEXT_BYTES);
    if clean.is_empty() {
        return Err(InfraError::Unavailable(format!("{operation}: empty value")));
    }
    Ok(clean)
}

fn read_display_text(path: &Path, operation: &'static str) -> Result<String, InfraError> {
    let text = read_identity_file(path, operation)?;
    let clean = sanitize_display(text.trim(), MAX_HOST_TEXT_BYTES);
    if clean.is_empty() {
        return Err(InfraError::Unavailable(format!("{operation}: empty value")));
    }
    Ok(clean)
}

/// PID namespace of `pid` from `readlink /proc/<pid>/ns/pid` (`pid:[N]`).
/// macOS has no PID namespaces: every lookup reports unavailable so the
/// action adapter degrades to its non-pidfd path instead of guessing.
pub fn pid_namespace_of(pid: i32) -> Result<PidNamespace, InfraError> {
    pid_namespace_at(Path::new("/"), pid)
}

/// [`pid_namespace_of`] with `/proc` resolved under `root`.
pub fn pid_namespace_at(root: &Path, pid: i32) -> Result<PidNamespace, InfraError> {
    #[cfg(not(target_os = "macos"))]
    {
        if pid <= 0 {
            return Err(InfraError::Unavailable(format!(
                "pid {pid} is outside 1..=2147483647"
            )));
        }
        let link: PathBuf = root.join(format!("proc/{pid}/ns/pid"));
        let target = std::fs::read_link(&link)
            .map_err(|error| InfraError::from_io("clock.pid_namespace", &error))?;
        let text = target
            .to_str()
            .ok_or_else(|| InfraError::Unavailable("pid namespace link is not UTF-8".into()))?;
        if !(text.starts_with("pid:[") && text.ends_with(']')) {
            return Err(InfraError::Unavailable(format!(
                "unexpected pid namespace link {}",
                sanitize_display(text, 64)
            )));
        }
        PidNamespace::new(text)
            .map_err(|error| InfraError::Unavailable(format!("pid namespace rejected: {error}")))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (root, pid);
        Err(InfraError::Unavailable(
            "pid namespaces are a Linux interface".into(),
        ))
    }
}

/// Effective uid of this process (caller context for action authorization).
pub fn current_euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn read_identity_file(path: &Path, operation: &'static str) -> Result<String, InfraError> {
    let file = File::open(path).map_err(|error| InfraError::from_io(operation, &error))?;
    let mut text = String::new();
    file.take(MAX_IDENTITY_FILE_BYTES)
        .read_to_string(&mut text)
        .map_err(|error| InfraError::from_io(operation, &error))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let unique = format!(
                "fox-services-top-runtime-clock-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn boot_clock_is_monotonic_and_has_a_boot_id() {
        let clock = BootClock::new().unwrap();
        assert!(!clock.boot_id().as_str().is_empty());
        let expected = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        assert_eq!(clock.boot_id().as_str(), expected.trim());
        assert_boot_clock_moves_forward(&clock);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn boot_clock_is_monotonic_and_has_a_macos_boot_id() {
        let clock = BootClock::new().unwrap();
        assert!(clock.boot_id().as_str().starts_with("macos-boot-"));
        assert_boot_clock_moves_forward(&clock);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assert_boot_clock_moves_forward(clock: &BootClock) {
        let mut previous = clock.now_boot_ns();
        assert!(previous > 0);
        for _ in 0..10_000 {
            let now = clock.now_boot_ns();
            assert!(now >= previous, "boot clock went backwards");
            previous = now;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(clock.now_boot_ns() > previous);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn missing_boot_id_is_an_error_not_a_default() {
        let root = TempRoot::new("noboot");
        assert!(BootClock::with_root(&root.0).is_err());
        root.write(BOOT_ID_PATH, "   \n");
        assert!(BootClock::with_root(&root.0).is_err());
        root.write(BOOT_ID_PATH, "1234-boot\n");
        assert_eq!(
            BootClock::with_root(&root.0).unwrap().boot_id().as_str(),
            "1234-boot"
        );
    }

    #[test]
    fn host_identity_hashes_machine_id_without_exposing_it() {
        let root = TempRoot::new("machineid");
        root.write("etc/machine-id", "abc\n");
        let id = host_identity(&root.0).unwrap();
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(id.as_str(), "host-ba7816bf8f01cfea");
        assert!(!id.as_str().contains("abc"));
    }

    #[test]
    fn host_identity_falls_back_to_dbus_then_hostname() {
        let root = TempRoot::new("fallback");
        #[cfg(target_os = "macos")]
        {
            // On macOS `kern.uuid` always answers, so the fake root still
            // yields a stable identity and the file fallbacks keep priority.
            let from_sysctl = host_identity(&root.0).unwrap();
            root.write(HOSTNAME_PATH, "node-7\n");
            assert_eq!(host_identity(&root.0).unwrap(), from_sysctl);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(host_identity(&root.0).is_err());
            root.write(HOSTNAME_PATH, "node-7\n");
            let from_hostname = host_identity(&root.0).unwrap();
            assert_eq!(
                from_hostname.as_str(),
                format!("host-{}", &sha256_hex(b"node-7")[..16])
            );
            root.write("var/lib/dbus/machine-id", "dbus-id\n");
            let from_dbus = host_identity(&root.0).unwrap();
            assert_eq!(
                from_dbus.as_str(),
                format!("host-{}", &sha256_hex(b"dbus-id")[..16])
            );
            // An empty /etc/machine-id is skipped, not hashed.
            root.write("etc/machine-id", "\n");
            assert_eq!(host_identity(&root.0).unwrap(), from_dbus);
        }
    }

    #[test]
    fn host_name_and_release_are_sanitized() {
        let root = TempRoot::new("hostname");
        root.write(HOSTNAME_PATH, "evil\u{1b}[31mhost\n");
        root.write(OSRELEASE_PATH, "6.12.0-test\n");
        assert_eq!(host_name_at(&root.0).unwrap(), "evilhost");
        assert_eq!(kernel_release_at(&root.0).unwrap(), "6.12.0-test");
        root.write(OSRELEASE_PATH, "\n");
        assert!(kernel_release_at(&root.0).is_err());
        assert!(!host_name().unwrap().is_empty());
        assert!(!kernel_release().unwrap().is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn pid_namespace_of_self_and_invalid_pids() {
        let own = i32::try_from(std::process::id()).unwrap();
        let namespace = pid_namespace_of(own).unwrap();
        assert!(namespace.as_str().starts_with("pid:["));
        assert!(pid_namespace_of(0).is_err());
        assert!(pid_namespace_of(-5).is_err());
        assert!(pid_namespace_of(i32::MAX).is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn pid_namespace_is_unavailable_on_macos() {
        let own = i32::try_from(std::process::id()).unwrap();
        assert!(pid_namespace_of(own).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn effective_uid_matches_proc_self_owner() {
        use std::os::unix::fs::MetadataExt;
        let owner = fs::metadata("/proc/self").unwrap().uid();
        assert_eq!(current_euid(), owner);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_host_identity_is_stable_and_hashed() {
        let first = host_identity(Path::new("/")).unwrap();
        let second = host_identity(Path::new("/")).unwrap();
        assert_eq!(first, second);
        assert!(first.as_str().starts_with("host-"));
        // No sysctl fallback on fake roots without machine files: on macOS
        // the live `kern.uuid` still anchors the identity.
        assert!(!host_name().unwrap().is_empty());
        assert!(!kernel_release().unwrap().is_empty());
    }
}
