//! `install.sh` against a local release directory (`--base-url file://…`) in
//! a temporary HOME and install directory: checksum and archive checks, the
//! version and target check, the host checks (non-Linux, WSL 1, musl, a too
//! old glibc) and the release checks (releases before v2, a missing archive),
//! the atomic replacement that keeps `fox-services-top.previous`, and rollback (AT18,
//! AT20). Fake `uname` and `ldd` tools on PATH stand in for
//! other hosts. Nothing touches the real `~/.local/bin` and nothing goes to
//! the network.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::BINARY;

const INSTALLER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh");

fn release_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => panic!("no release target for {os}/{arch}"),
    }
}

/// The Linux build for this CPU: the pinned-Linux tests (musl, WSL) fake
/// `uname -s`, so the archive must use the Linux triple even on macOS.
fn linux_target() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        arch => panic!("no linux target for {arch}"),
    }
}

fn current_tag() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// A temporary HOME with a release directory and an install directory.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fox-services-top-installer-{label}-{}-{unique}",
            std::process::id()
        ));
        for directory in ["release", "bin", "staging", "tools"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        Self { root }
    }

    fn release_dir(&self) -> PathBuf {
        self.root.join("release")
    }

    fn install_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn installed(&self) -> PathBuf {
        self.install_dir().join("fox-services-top")
    }

    fn previous(&self) -> PathBuf {
        self.install_dir().join("fox-services-top.previous")
    }

    fn archive_path_as(&self, tag: &str, target: &str) -> PathBuf {
        self.release_dir()
            .join(format!("fox-services-top-{tag}-{target}.tar.gz"))
    }

    /// Publishes `binary` (plus `extra` entries) as the archive of `tag`,
    /// with a matching `SHA256SUMS`.
    fn publish(&self, tag: &str, binary: &Path, extra: &[&str]) {
        self.publish_as(tag, binary, extra, release_target());
    }

    /// Like [`Sandbox::publish`] with an explicit target triple (the
    /// pinned-Linux tests fake `uname -s` on every host).
    fn publish_as(&self, tag: &str, binary: &Path, extra: &[&str], target: &str) {
        let staging = self.root.join("staging");
        std::fs::copy(binary, staging.join("fox-services-top")).unwrap();
        let mut entries = vec!["fox-services-top".to_owned()];
        for name in extra {
            std::fs::write(staging.join(name), b"extra").unwrap();
            entries.push((*name).to_owned());
        }
        let archive = self.archive_path_as(tag, target);
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .args(&entries)
            .status()
            .unwrap();
        assert!(status.success(), "tar failed");
        // `sha256sum` on Linux, `shasum -a 256` on stock macOS.
        let sums = Command::new("sha256sum")
            .arg(archive.file_name().unwrap())
            .current_dir(self.release_dir())
            .output()
            .unwrap();
        let sums = if sums.status.success() {
            sums.stdout
        } else {
            let fallback = Command::new("shasum")
                .args(["-a", "256", &archive.file_name().unwrap().to_string_lossy()])
                .current_dir(self.release_dir())
                .output()
                .unwrap();
            assert!(fallback.status.success(), "shasum failed");
            fallback.stdout
        };
        std::fs::write(self.release_dir().join("SHA256SUMS"), sums).unwrap();
    }

    /// Places an executable script named `name` first on PATH.
    fn add_tool(&self, name: &str, script: &str) {
        let tool = self.root.join("tools").join(name);
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake `ldd` that behaves like musl's: `ldd --version` prints the
    /// banner on stderr and exits 1.
    fn pretend_musl(&self) {
        self.add_tool(
            "ldd",
            "#!/bin/sh\necho 'musl libc (x86_64)' >&2\necho 'Version 1.2.5' >&2\nexit 1\n",
        );
    }

    /// A fake `uname` that answers each `flag` in `answers` with its value
    /// and passes every other question to the real `uname`.
    fn pretend_unames(&self, answers: &[(&str, &str)]) {
        let real = ["/usr/bin/uname", "/bin/uname"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .expect("a real uname");
        let mut script = String::from("#!/bin/sh\n");
        for (flag, value) in answers {
            script.push_str(&format!(
                "if [ \"$1\" = '{flag}' ]; then echo '{value}'; exit 0; fi\n"
            ));
        }
        script.push_str(&format!("exec {real} \"$@\"\n"));
        self.add_tool("uname", &script);
    }

    /// A fake `uname` that answers `flag` with `value` and passes every other
    /// question to the real `uname`.
    fn pretend_uname(&self, flag: &str, value: &str) {
        self.pretend_unames(&[(flag, value)]);
    }

    /// A "binary" that reports this version and target, then far more lines
    /// than a pipe holds, as a long verbose report would.
    fn binary_with_a_long_report(&self) -> PathBuf {
        let path = self.root.join("long-report");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho 'fox-services-top {}'\necho 'target: {}'\ni=0\nwhile [ $i -lt 5000 ]; do echo \"detail line $i of a long verbose report\"; i=$((i+1)); done\n",
                env!("CARGO_PKG_VERSION"),
                release_target()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A "binary" that fails to load the way a build for a newer glibc does.
    fn binary_for_a_newer_glibc(&self) -> PathBuf {
        let path = self.root.join("needs-newer-glibc");
        std::fs::write(
            &path,
            "#!/bin/sh\necho 'fox-services-top: libc.so.6: version GLIBC_2.99 not found (required by fox-services-top)' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A previously installed "binary" that reports another version.
    fn install_previous_release(&self) {
        let script = "#!/bin/sh\necho 'fox-services-top 0.1.0'\n";
        std::fs::write(self.installed(), script).unwrap();
        std::fs::set_permissions(self.installed(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_installer(&self, arguments: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .arg(INSTALLER)
            .args(arguments)
            .arg("--install-dir")
            .arg(self.install_dir())
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    fn install(&self, tag: &str) -> Output {
        let base = format!("file://{}", self.release_dir().display());
        self.run_installer(&["--version", tag, "--base-url", &base])
    }

    fn rollback(&self) -> Output {
        self.run_installer(&["--rollback"])
    }

    /// Everything left in the install directory (staging must be gone).
    fn install_dir_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.install_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A script standing in for a Linux binary's `--version --verbose` report
/// (used where the pinned-Linux branch runs on a non-Linux host).
#[cfg(not(target_os = "linux"))]
fn linux_report_stub(sandbox: &Sandbox) -> PathBuf {
    let path = sandbox.root.join("linux-report-stub");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = '--version' ] && [ \"$2\" = '--verbose' ]; then\n  echo 'fox-services-top {v}'\n  echo 'target: {t}'\nelse\n  echo 'fox-services-top {v}'\nfi\n",
        v = env!("CARGO_PKG_VERSION"),
        t = linux_target()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn reported_version(binary: &Path) -> String {
    let output = Command::new(binary).arg("--version").output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn installs_a_verified_release_keeps_the_previous_binary_and_rolls_back() {
    let sandbox = Sandbox::new("roundtrip");
    let config = sandbox.root.join(".config/fox-services-top/v2/config.json");
    let audit = sandbox
        .root
        .join(".local/state/fox-services-top/v2/audit.sqlite3");
    for file in [&config, &audit] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"user data").unwrap();
    }
    sandbox.install_previous_release();
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);

    let installed = sandbox.install(&current_tag());
    assert!(installed.status.success(), "{}", stderr_of(&installed));
    assert_eq!(
        reported_version(&sandbox.installed()),
        format!("fox-services-top {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        reported_version(&sandbox.previous()),
        "fox-services-top 0.1.0"
    );
    assert_eq!(
        sandbox.install_dir_entries(),
        ["fox-services-top", "fox-services-top.previous"],
        "no staging directory is left behind"
    );

    let rolled_back = sandbox.rollback();
    assert!(rolled_back.status.success(), "{}", stderr_of(&rolled_back));
    assert_eq!(
        reported_version(&sandbox.installed()),
        "fox-services-top 0.1.0"
    );
    assert_eq!(
        reported_version(&sandbox.previous()),
        format!("fox-services-top {}", env!("CARGO_PKG_VERSION"))
    );
    let message = String::from_utf8_lossy(&rolled_back.stdout);
    assert!(message.contains("configuration and state were not modified"));
    assert!(message.contains("signals already sent to processes cannot be rolled back"));
    for file in [&config, &audit] {
        assert_eq!(std::fs::read(file).unwrap(), b"user data");
    }
}

#[test]
fn a_checksum_mismatch_is_refused_and_the_installed_binary_is_kept() {
    let sandbox = Sandbox::new("checksum");
    sandbox.install_previous_release();
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);
    let sums = sandbox.release_dir().join("SHA256SUMS");
    let tampered =
        std::fs::read_to_string(&sums)
            .unwrap()
            .replacen(|c: char| c.is_ascii_hexdigit(), "0", 8);
    std::fs::write(&sums, tampered).unwrap();
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("checksum mismatch"),
        "{}",
        stderr_of(&refused)
    );
    assert_eq!(
        reported_version(&sandbox.installed()),
        "fox-services-top 0.1.0"
    );
    assert_eq!(sandbox.install_dir_entries(), ["fox-services-top"]);
}

#[test]
fn a_binary_reporting_another_version_is_refused() {
    let sandbox = Sandbox::new("version");
    sandbox.publish("v9.9.9", Path::new(BINARY), &[]);
    let refused = sandbox.install("v9.9.9");
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("binary version does not match"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

/// The version and target lines are matched in the whole report. Read from a
/// pipe under pipefail, a `grep -q` that stopped at the first line ended the
/// writer of the rest with SIGPIPE and refused a release whose version matched.
#[test]
fn a_long_version_report_is_matched_without_a_broken_pipe() {
    let sandbox = Sandbox::new("long-report");
    let binary = sandbox.binary_with_a_long_report();
    sandbox.publish(&current_tag(), &binary, &[]);
    let installed = sandbox.install(&current_tag());
    assert!(installed.status.success(), "{}", stderr_of(&installed));
    assert_eq!(sandbox.install_dir_entries(), ["fox-services-top"]);
}

#[test]
fn a_requested_version_that_only_prefixes_the_binary_version_is_refused() {
    let sandbox = Sandbox::new("version-prefix");
    let version = env!("CARGO_PKG_VERSION");
    let prefix = format!("v{}", &version[..version.rfind('.').unwrap()]);
    sandbox.publish(&prefix, Path::new(BINARY), &[]);
    let refused = sandbox.install(&prefix);
    assert!(
        !refused.status.success(),
        "{prefix} accepted fox-services-top {version}"
    );
    assert!(
        stderr_of(&refused).contains("binary version does not match"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn an_archive_without_a_checksum_entry_is_refused() {
    let sandbox = Sandbox::new("no-sum");
    sandbox.install_previous_release();
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);
    let sums = sandbox.release_dir().join("SHA256SUMS");
    let other_file = std::fs::read_to_string(&sums)
        .unwrap()
        .replace(".tar.gz", ".tar.gz.old");
    std::fs::write(&sums, other_file).unwrap();
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("SHA256SUMS has no entry"),
        "{}",
        stderr_of(&refused)
    );
    assert_eq!(
        reported_version(&sandbox.installed()),
        "fox-services-top 0.1.0"
    );
    assert_eq!(sandbox.install_dir_entries(), ["fox-services-top"]);
}

#[test]
fn an_archive_with_more_than_the_binary_is_refused() {
    let sandbox = Sandbox::new("layout");
    sandbox.publish(&current_tag(), Path::new(BINARY), &["README"]);
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("exactly one entry"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn musl_hosts_are_refused_without_a_fallback() {
    let sandbox = Sandbox::new("musl");
    // musl is a Linux-only concept: pin the Linux branch on every host.
    sandbox.pretend_unames(&[("-s", "Linux")]);
    sandbox.pretend_musl();
    sandbox.publish_as(&current_tag(), Path::new(BINARY), &[], linux_target());
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("no musl fallback"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn plain_http_release_urls_are_refused() {
    let sandbox = Sandbox::new("http");
    let refused = sandbox.run_installer(&[
        "--version",
        &current_tag(),
        "--base-url",
        "http://example.invalid/release",
    ]);
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("refusing non-https URL"),
        "{}",
        stderr_of(&refused)
    );
}

#[test]
fn rollback_without_a_previous_binary_fails_cleanly() {
    let sandbox = Sandbox::new("no-previous");
    let refused = sandbox.rollback();
    assert!(!refused.status.success());
    assert!(stderr_of(&refused).contains("no previous binary"));
}

#[test]
fn non_linux_hosts_are_refused_before_anything_is_installed() {
    let sandbox = Sandbox::new("unsupported-os");
    // Neither the Linux nor the macOS branch accepts this system; Darwin
    // itself is a supported host and is covered by every other test on it.
    sandbox.pretend_uname("-s", "FreeBSD");
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused)
            .contains("supports Linux and macOS (Apple Silicon) only (found FreeBSD)"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn wsl1_hosts_are_refused() {
    let sandbox = Sandbox::new("wsl1");
    // WSL is a Linux-only concept: pin the Linux branch on every host.
    sandbox.pretend_unames(&[("-s", "Linux"), ("-r", "4.4.0-19041-Microsoft")]);
    sandbox.publish_as(&current_tag(), Path::new(BINARY), &[], linux_target());
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("WSL 1 is not supported"),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn wsl2_hosts_get_the_linux_build_and_a_driver_note() {
    let sandbox = Sandbox::new("wsl2");
    // WSL is a Linux-only concept: pin the Linux branch on every host.
    sandbox.pretend_unames(&[
        ("-s", "Linux"),
        ("-r", "5.15.153.1-microsoft-standard-WSL2"),
    ]);
    // The archived binary must report the pinned Linux target: the real
    // binary on Linux, a script with the matching report on macOS (where the
    // real binary reports the Darwin target).
    #[cfg(target_os = "linux")]
    let published: &Path = Path::new(BINARY);
    #[cfg(not(target_os = "linux"))]
    let published: &Path = &linux_report_stub(&sandbox);
    sandbox.publish_as(&current_tag(), published, &[], linux_target());
    let installed = sandbox.install(&current_tag());
    assert!(installed.status.success(), "{}", stderr_of(&installed));
    assert!(
        String::from_utf8_lossy(&installed.stdout).contains("WSL 2 detected"),
        "{}",
        String::from_utf8_lossy(&installed.stdout)
    );
    assert_eq!(
        reported_version(&sandbox.installed()),
        format!("fox-services-top {}", env!("CARGO_PKG_VERSION"))
    );
}

/// Apple Silicon hosts install the `aarch64-apple-darwin` build through the
/// same verified path (macOS-only: the Darwin branch is unreachable on Linux).
#[test]
#[cfg(target_os = "macos")]
fn macos_hosts_install_the_apple_silicon_build() {
    let sandbox = Sandbox::new("darwin-target");
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);
    let installed = sandbox.install(&current_tag());
    assert!(installed.status.success(), "{}", stderr_of(&installed));
    assert!(
        String::from_utf8_lossy(&installed.stdout).contains("aarch64-apple-darwin"),
        "{}",
        String::from_utf8_lossy(&installed.stdout)
    );
    assert_eq!(
        reported_version(&sandbox.installed()),
        format!("fox-services-top {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn releases_before_v2_are_refused_before_downloading() {
    for old in ["v0.2.5", "0.1.0", "v1.0.0"] {
        let sandbox = Sandbox::new("old-release");
        let refused = sandbox.install(old);
        assert!(!refused.status.success(), "{old} was accepted");
        let stderr = stderr_of(&refused);
        assert!(
            stderr.contains("earlier fox-services-top"),
            "{old}: {stderr}"
        );
        assert!(!stderr.contains("could not download"), "{old}: {stderr}");
        assert!(sandbox.install_dir_entries().is_empty());
    }
}

#[test]
fn a_version_without_the_v_prefix_installs_that_release() {
    let sandbox = Sandbox::new("no-v-prefix");
    sandbox.publish(&current_tag(), Path::new(BINARY), &[]);
    let installed = sandbox.install(env!("CARGO_PKG_VERSION"));
    assert!(installed.status.success(), "{}", stderr_of(&installed));
    assert_eq!(
        reported_version(&sandbox.installed()),
        format!("fox-services-top {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn a_missing_release_names_the_archive_it_could_not_download() {
    let sandbox = Sandbox::new("missing");
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    let expected = format!(
        "could not download fox-services-top-{}-{}.tar.gz",
        current_tag(),
        release_target()
    );
    assert!(
        stderr_of(&refused).contains(&expected),
        "{}",
        stderr_of(&refused)
    );
    assert!(sandbox.install_dir_entries().is_empty());
}

#[test]
fn a_build_for_a_newer_glibc_is_reported_as_such() {
    let sandbox = Sandbox::new("glibc");
    sandbox.install_previous_release();
    let binary = sandbox.binary_for_a_newer_glibc();
    sandbox.publish(&current_tag(), &binary, &[]);
    let refused = sandbox.install(&current_tag());
    assert!(!refused.status.success());
    assert!(
        stderr_of(&refused).contains("needs a newer glibc"),
        "{}",
        stderr_of(&refused)
    );
    assert_eq!(
        reported_version(&sandbox.installed()),
        "fox-services-top 0.1.0"
    );
    assert_eq!(sandbox.install_dir_entries(), ["fox-services-top"]);
}
