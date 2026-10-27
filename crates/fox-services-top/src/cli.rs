//! Command line.
//!
//! Every overridable setting is an `Option`: `None` means "not given" and
//! must never overwrite a persisted value.

use std::path::PathBuf;

use clap::{ArgGroup, Parser};
use fox_services_top_core::config::{CliOverrides, RemoteNodeConfig, Theme};
use thiserror::Error;

/// Terminal GPU monitor for NVIDIA DGX Spark and Linux machines with NVIDIA GPUs.
#[derive(Debug, Clone, Parser)]
#[command(name = "fox-services-top", about, disable_version_flag = true)]
#[command(group(ArgGroup::new("import_mode").args(["dry_run", "apply"]).multiple(false)))]
pub struct Cli {
    /// Sampling interval in seconds (0.1-60); overrides the saved value for this run only.
    #[arg(short = 'i', long, value_name = "SECONDS")]
    pub interval: Option<f64>,

    /// Disable the NVML adapter host for this run.
    #[arg(long)]
    pub no_gpu: bool,

    /// Colour theme for this run: dark, cyan, green or amber.
    #[arg(short = 't', long, value_name = "NAME")]
    pub theme: Option<String>,

    /// Maximum network interfaces shown (1-20); display only.
    #[arg(long, value_name = "N")]
    pub net_max: Option<u32>,

    /// Disable all process actions for this run.
    #[arg(long)]
    pub read_only: bool,

    /// A fox-services node to poll for the Nodes and Docker tabs,
    /// `NAME=URL` (repeatable, e.g. `--remote-node axiom=http://192.168.1.63:8210`);
    /// replaces the configured node list for this run only.
    #[arg(long, value_name = "NAME=URL")]
    pub remote_node: Vec<String>,

    /// Disable remote fox-services monitoring for this run.
    #[arg(long)]
    pub no_remote: bool,

    /// Absolute path of the v2 configuration file.
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    pub config: Option<PathBuf>,

    /// Absolute path of the state directory (audit, archive, log, lock).
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    pub state_dir: Option<PathBuf>,

    /// Validate the configuration and print configured/effective values; samples nothing, writes nothing.
    #[arg(long)]
    pub check_config: bool,

    /// Print version, dependency, SQLite, capability and bounded health information as JSON.
    #[arg(long)]
    pub diagnostics_json: bool,

    /// Import a legacy (v1) configuration file; requires --dry-run or --apply.
    #[arg(long, value_name = "PATH", requires = "import_mode")]
    pub import_legacy: Option<PathBuf>,

    /// With --import-legacy: show the complete mapping without writing.
    #[arg(long, requires = "import_legacy")]
    pub dry_run: bool,

    /// With --import-legacy: write the mapped v2 configuration (refuses to overwrite an existing v2 file).
    #[arg(long, requires = "import_legacy")]
    pub apply: bool,

    /// Run against a replay fixture (simulated data only, read-only, separate temporary state).
    #[arg(long, value_name = "FIXTURE")]
    pub replay: Option<PathBuf>,

    /// Run headless: keep collecting and persisting with no terminal, and
    /// serve attaching viewers (`fox-services-top` without `--daemon`) over a
    /// local socket in the state directory. Intended for a systemd user
    /// service so history keeps accumulating while no viewer is open.
    #[arg(long)]
    pub daemon: bool,

    /// Start a standalone run even when a `--daemon` collector is running:
    /// never attach to its socket (the run then fails if its lock is held).
    #[arg(long)]
    pub no_attach: bool,

    /// Print version information (add --verbose for build and runtime details).
    #[arg(short = 'V', long)]
    pub version: bool,

    /// With --version: include commit, toolchain, target, profile and SQLite runtime.
    #[arg(long, requires = "version")]
    pub verbose: bool,

    /// Internal: run as a managed adapter host (only valid behind the supervisor's pipe handshake).
    #[arg(long, hide = true, value_name = "ADAPTER_ID")]
    pub adapter_host: Option<String>,

    /// Internal: generation assigned to the adapter host by the supervisor.
    #[arg(long, hide = true, value_name = "N")]
    pub host_generation: Option<u32>,

    /// Internal: replay fixture served by a replay adapter host.
    #[arg(long, hide = true, value_name = "FIXTURE")]
    pub replay_fixture: Option<PathBuf>,

    /// Internal: `INDEX/COUNT` slice of the replay fixture served by this
    /// host (the live adapter families of that slice).
    #[arg(long, hide = true, value_name = "INDEX/COUNT")]
    pub replay_slice: Option<String>,
}

/// Invalid command line values (reported before anything starts).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CliError {
    #[error("--interval must be a finite number of seconds within 0.1..=60 (got {0})")]
    Interval(String),
    #[error("--theme must be one of dark, cyan, green, amber (got {0})")]
    Theme(String),
    #[error("--net-max must be within 1..=20 (got {0})")]
    NetMax(u32),
    #[error("{flag} must be an absolute path (got {path})")]
    RelativePath { flag: &'static str, path: String },
    #[error("--replay-slice must look like INDEX/COUNT with INDEX < COUNT (got {0})")]
    ReplaySlice(String),
    #[error("--remote-node must look like NAME=URL with an http(s) URL (got {0})")]
    RemoteNode(String),
}

impl Cli {
    /// Explicit overrides only; absent flags stay `None`.
    pub fn overrides(&self) -> Result<CliOverrides, CliError> {
        Ok(CliOverrides {
            interval_ms: self.interval.map(interval_seconds_to_ms).transpose()?,
            gpu_enabled: self.no_gpu.then_some(false),
            theme: self
                .theme
                .as_deref()
                .map(|name| Theme::parse(name).ok_or_else(|| CliError::Theme(name.to_owned())))
                .transpose()?,
            network_max_visible: self
                .net_max
                .map(|visible| {
                    if (1..=20).contains(&visible) {
                        Ok(visible)
                    } else {
                        Err(CliError::NetMax(visible))
                    }
                })
                .transpose()?,
            read_only: (self.read_only || self.replay.is_some()).then_some(true),
            remote_enabled: self.no_remote.then_some(false),
            remote_nodes: parse_remote_nodes(&self.remote_node)?,
        })
    }

    /// Validates path flags (absolute only; symlink/owner checks happen at open).
    pub fn validate_paths(&self) -> Result<(), CliError> {
        for (flag, path) in [("--config", &self.config), ("--state-dir", &self.state_dir)] {
            if let Some(path) = path
                && !path.is_absolute()
            {
                return Err(CliError::RelativePath {
                    flag,
                    path: path.display().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Parsed `--replay-slice`.
    pub fn replay_slice(&self) -> Result<Option<(u32, u32)>, CliError> {
        let Some(text) = self.replay_slice.as_deref() else {
            return Ok(None);
        };
        let parsed = text
            .split_once('/')
            .and_then(|(index, count)| {
                Some((index.parse::<u32>().ok()?, count.parse::<u32>().ok()?))
            })
            .filter(|(index, count)| index < count);
        parsed
            .map(Some)
            .ok_or_else(|| CliError::ReplaySlice(text.to_owned()))
    }
}

/// Parses `--remote-node NAME=URL` flags; `None` when none were given.
fn parse_remote_nodes(entries: &[String]) -> Result<Option<Vec<RemoteNodeConfig>>, CliError> {
    if entries.is_empty() {
        return Ok(None);
    }
    entries
        .iter()
        .map(|entry| {
            let (name, url) = entry
                .split_once('=')
                .filter(|(name, url)| {
                    !name.is_empty() && (url.starts_with("http://") || url.starts_with("https://"))
                })
                .ok_or_else(|| CliError::RemoteNode(entry.clone()))?;
            Ok(RemoteNodeConfig {
                name: name.to_owned(),
                base_url: url.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Converts seconds to whole milliseconds and checks the schema range.
fn interval_seconds_to_ms(seconds: f64) -> Result<u32, CliError> {
    if !seconds.is_finite() {
        return Err(CliError::Interval(seconds.to_string()));
    }
    let milliseconds = (seconds * 1000.0).round();
    if !(100.0..=60_000.0).contains(&milliseconds) {
        return Err(CliError::Interval(seconds.to_string()));
    }
    // Range-checked above, so the conversion is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(milliseconds as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Cli {
        let mut full = vec!["fox-services-top"];
        full.extend_from_slice(arguments);
        Cli::try_parse_from(full).unwrap()
    }

    #[test]
    fn absent_flags_produce_no_overrides() {
        let overrides = parse(&[]).overrides().unwrap();
        assert_eq!(
            overrides,
            CliOverrides::default(),
            "no flag may override persisted settings"
        );
    }

    #[test]
    fn explicit_flags_override() {
        let overrides = parse(&[
            "--interval",
            "0.25",
            "--no-gpu",
            "--theme",
            "amber",
            "--net-max",
            "5",
            "--read-only",
        ])
        .overrides()
        .unwrap();
        assert_eq!(overrides.interval_ms, Some(250));
        assert_eq!(overrides.gpu_enabled, Some(false));
        assert_eq!(overrides.theme, Some(Theme::Amber));
        assert_eq!(overrides.network_max_visible, Some(5));
        assert_eq!(overrides.read_only, Some(true));
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(matches!(
            parse(&["--interval", "0.01"]).overrides(),
            Err(CliError::Interval(_))
        ));
        assert!(matches!(
            parse(&["--interval", "NaN"]).overrides(),
            Err(CliError::Interval(_))
        ));
        assert!(matches!(
            parse(&["--interval", "61"]).overrides(),
            Err(CliError::Interval(_))
        ));
        assert!(matches!(
            parse(&["--theme", "neon"]).overrides(),
            Err(CliError::Theme(_))
        ));
        assert!(matches!(
            parse(&["--net-max", "0"]).overrides(),
            Err(CliError::NetMax(0))
        ));
        assert!(
            parse(&["--config", "relative.json"])
                .validate_paths()
                .is_err()
        );
        assert!(
            parse(&["--config", "/abs/config.json"])
                .validate_paths()
                .is_ok()
        );
    }

    #[test]
    fn the_removed_log_level_flag_is_refused() {
        // D-93: the log file has no level filter, so the flag is gone.
        for level in ["warn", "debug", "trace"] {
            assert!(Cli::try_parse_from(["fox-services-top", "--log-level", level]).is_err());
        }
    }

    #[test]
    fn replay_forces_read_only() {
        let overrides = parse(&["--replay", "/tmp/fixture.json"])
            .overrides()
            .unwrap();
        assert_eq!(overrides.read_only, Some(true));
    }

    #[test]
    fn remote_nodes_parse_and_disable() {
        let overrides = parse(&[
            "--remote-node",
            "axiom=http://192.168.1.63:8210",
            "--remote-node",
            "axiom-dgx=http://192.168.1.200:8210",
        ])
        .overrides()
        .unwrap();
        assert_eq!(
            overrides.remote_nodes,
            Some(vec![
                fox_services_top_core::config::RemoteNodeConfig {
                    name: "axiom".into(),
                    base_url: "http://192.168.1.63:8210".into(),
                },
                fox_services_top_core::config::RemoteNodeConfig {
                    name: "axiom-dgx".into(),
                    base_url: "http://192.168.1.200:8210".into(),
                },
            ])
        );
        assert_eq!(overrides.remote_enabled, None);
        let off = parse(&["--no-remote"]).overrides().unwrap();
        assert_eq!(off.remote_enabled, Some(false));
        assert_eq!(off.remote_nodes, None);
        assert!(matches!(
            parse(&["--remote-node", "axiom"]).overrides(),
            Err(CliError::RemoteNode(_))
        ));
        assert!(matches!(
            parse(&["--remote-node", "=http://x/"]).overrides(),
            Err(CliError::RemoteNode(_))
        ));
        assert!(matches!(
            parse(&["--remote-node", "axiom=gopher://x"]).overrides(),
            Err(CliError::RemoteNode(_))
        ));
    }
}
