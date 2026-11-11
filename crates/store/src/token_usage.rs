//! The token usage log: a SQLite database (`token_usage.sqlite3`, its own
//! `user_version` 1, format 1) holding one row per poll per (node, app,
//! model) with the windowed totals the node reported, written by the
//! background remote monitor. A timeseries over `utc_ns` shows consumption;
//! rows older than the configured retention are pruned on append.
//!
//! Opened like the other partitions (0600, no symlinks, format, schema and
//! integrity checked before any write, a file of another format recreated
//! (D-85), `max_page_count`); one still unusable makes every call fail with
//! its reason while polling goes on in memory. A failed call maps like
//! every partition's (`sqlite::Writer`): corruption stops the database
//! until the next start recreates it.

use std::path::Path;
use std::time::Duration;

use fox_services_top_core::errors::InfraError;
use rusqlite::{Connection, params};

use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    DatabaseFailure, SchemaKind, VersionGate, Writer, ensure_sqlite_version, immediate_transaction,
    open_small_database,
};

/// Retries a busy database this long per call.
const BUSY_DEADLINE: Duration = Duration::from_millis(500);

/// One poll's windowed token flow for a single (node, app, model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSample {
    /// Wall clock of the poll, Unix nanoseconds.
    pub utc_ns: i64,
    /// Node name as configured (`axiom`).
    pub node: String,
    /// App/service name as the node reported it.
    pub app: String,
    /// Model as the node reported it.
    pub model: String,
    pub requests: u64,
    pub ingress_tokens: u64,
    pub egress_tokens: u64,
}

/// What the log holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub rows: u64,
    /// Oldest and newest sample held, Unix nanoseconds.
    pub oldest_utc_ns: Option<i64>,
    pub newest_utc_ns: Option<i64>,
}

/// The token usage log in fox-services-top's state directory.
#[derive(Debug)]
pub struct TokenUsageLog {
    database: Writer,
    recreated: Option<String>,
}

impl TokenUsageLog {
    /// Opens (creating when missing) the database at `path`. Never fails:
    /// an unusable database is reported by every call.
    pub fn open(path: &Path) -> Self {
        Self::open_with(path, &RealFileSystem, ensure_sqlite_version)
    }

    pub(crate) fn open_with(path: &Path, fs: &dyn FileSystemOps, gate: VersionGate) -> Self {
        let opened = open_small_database(path, SchemaKind::TokenUsage, fs, gate);
        let (database, recreated) = Writer::new("token usage", opened);
        Self {
            database,
            recreated,
        }
    }

    /// Why the database found at open was deleted and created afresh
    /// (D-85), if it was.
    pub fn recreated(&self) -> Option<&str> {
        self.recreated.as_deref()
    }

    fn connection(&mut self) -> Result<&mut Connection, InfraError> {
        self.database.connection().map_err(|unusable| {
            InfraError::Unavailable(format!("token usage database: {}", unusable.reason))
        })
    }

    /// Runs `call` on the database, mapping its failure (D-85: corruption
    /// stops the database).
    fn with_database<T>(
        &mut self,
        call: impl FnOnce(&mut Connection) -> Result<T, DatabaseFailure>,
    ) -> Result<T, InfraError> {
        let result = call(self.connection()?);
        result.map_err(|failure| self.database.fail(&failure))
    }

    /// Appends `samples` (one row per poll per series; re-polling the same
    /// timestamp replaces it) and prunes rows older than `retain_utc_ns`,
    /// returning what the log holds then.
    pub fn append(
        &mut self,
        samples: &[TokenSample],
        retain_utc_ns: i64,
    ) -> Result<TokenUsage, InfraError> {
        self.with_database(|connection| {
            immediate_transaction(connection, BUSY_DEADLINE, |transaction| {
                for sample in samples {
                    transaction
                        .prepare_cached(
                            "INSERT INTO token_samples \
                             (utc_ns, node, app, model, requests, ingress_tokens, egress_tokens) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                             ON CONFLICT (utc_ns, node, app, model) DO UPDATE SET \
                             requests = excluded.requests, \
                             ingress_tokens = excluded.ingress_tokens, \
                             egress_tokens = excluded.egress_tokens",
                        )?
                        .execute(params![
                            sample.utc_ns,
                            sample.node,
                            sample.app,
                            sample.model,
                            i64::try_from(sample.requests).unwrap_or(i64::MAX),
                            i64::try_from(sample.ingress_tokens).unwrap_or(i64::MAX),
                            i64::try_from(sample.egress_tokens).unwrap_or(i64::MAX),
                        ])?;
                }
                transaction
                    .prepare_cached("DELETE FROM token_samples WHERE utc_ns < ?1")?
                    .execute(params![retain_utc_ns])?;
                Ok(usage(transaction)?)
            })
            .map_err(DatabaseFailure::transaction("token usage.append"))
        })
    }

    /// What the log holds.
    pub fn usage(&mut self) -> Result<TokenUsage, InfraError> {
        self.with_database(|connection| {
            usage(connection).map_err(DatabaseFailure::statement("token usage.usage"))
        })
    }
}

fn usage(connection: &Connection) -> rusqlite::Result<TokenUsage> {
    let rows: i64 = connection
        .prepare_cached("SELECT count(*) FROM token_samples")?
        .query_row([], |row| row.get(0))?;
    let (oldest_utc_ns, newest_utc_ns): (Option<i64>, Option<i64>) = connection
        .prepare_cached("SELECT min(utc_ns), max(utc_ns) FROM token_samples")?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(TokenUsage {
        rows: u64::try_from(rows).unwrap_or(0),
        oldest_utc_ns,
        newest_utc_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fox-services-top-token-usage-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            // Like the real state directory: a file is recreated only in a
            // private one (D-85).
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
            Self(dir)
        }

        fn open(&self) -> TokenUsageLog {
            TokenUsageLog::open(&self.0.join("token_usage.sqlite3"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample(utc_ns: i64, app: &str, model: &str, ingress: u64, egress: u64) -> TokenSample {
        TokenSample {
            utc_ns,
            node: "axiom".to_owned(),
            app: app.to_owned(),
            model: model.to_owned(),
            requests: 7,
            ingress_tokens: ingress,
            egress_tokens: egress,
        }
    }

    const DAY_NS: i64 = 24 * 3_600 * 1_000_000_000;

    #[test]
    fn append_then_read_back() {
        let scratch = Scratch::new("append");
        let mut log = scratch.open();
        assert_eq!(log.usage().unwrap(), TokenUsage::default());
        let held = log
            .append(
                &[
                    sample(1_000, "quai-radar", "qwen:7b", 100, 50),
                    sample(1_000, "quai-radar", "qwen:27b", 200, 60),
                ],
                0,
            )
            .unwrap();
        assert_eq!(held.rows, 2);
        assert_eq!(held.oldest_utc_ns, Some(1_000));
        assert_eq!(held.newest_utc_ns, Some(1_000));
    }

    #[test]
    fn same_timestamp_replaces() {
        let scratch = Scratch::new("replace");
        let mut log = scratch.open();
        log.append(&[sample(1_000, "a", "m", 100, 50)], 0).unwrap();
        let held = log.append(&[sample(1_000, "a", "m", 300, 70)], 0).unwrap();
        assert_eq!(held.rows, 1, "re-polling replaces the row");
        assert_eq!(held.oldest_utc_ns, Some(1_000));
    }

    #[test]
    fn old_rows_are_pruned_on_append() {
        let scratch = Scratch::new("prune");
        let mut log = scratch.open();
        log.append(&[sample(1_000, "a", "m", 100, 50)], 0).unwrap();
        let held = log
            .append(&[sample(2 * DAY_NS, "a", "m", 100, 50)], DAY_NS)
            .unwrap();
        assert_eq!(held.rows, 1);
        assert_eq!(held.oldest_utc_ns, Some(2 * DAY_NS));
    }

    #[test]
    fn garbage_file_is_recreated() {
        let scratch = Scratch::new("garbage");
        let path = scratch.0.join("token_usage.sqlite3");
        std::fs::write(&path, b"not a database").unwrap();
        let mut log = TokenUsageLog::open(&path);
        assert!(log.recreated().is_some());
        log.append(&[sample(1_000, "a", "m", 1, 1)], 0).unwrap();
        assert_eq!(log.usage().unwrap().rows, 1);
    }
}
