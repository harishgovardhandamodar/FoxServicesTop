-- audit.sqlite3 only. Connections still verify the SQLite version, WAL and synchronous=FULL; this DDL never changes journal_mode.
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS action_intents (
 request_id TEXT PRIMARY KEY,
 intent_id TEXT NOT NULL UNIQUE,
 session_digest TEXT NOT NULL,
 process_key_json TEXT NOT NULL,
 signal TEXT NOT NULL CHECK(signal IN ('SIGTERM','SIGKILL')),
 policy_revision INTEGER NOT NULL CHECK(policy_revision>0),
 created_utc TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('intent_committed','sending','signal_sent','exit_observed','still_running','cancelled','expired','rejected','unknown')),
 payload_sha256 TEXT NOT NULL CHECK(length(payload_sha256)=64),
 boot_id TEXT NOT NULL,
 created_boot_ns TEXT NOT NULL CHECK(length(created_boot_ns)=20 AND created_boot_ns NOT GLOB '*[^0-9]*' AND created_boot_ns<='18446744073709551615'),
 CHECK(length(process_key_json)<=4096)
);
CREATE TABLE IF NOT EXISTS action_events (
 event_id TEXT PRIMARY KEY,
 request_id TEXT NOT NULL REFERENCES action_intents(request_id),
 occurred_utc TEXT NOT NULL,
 boot_id TEXT NOT NULL,
 occurred_boot_ns TEXT NOT NULL CHECK(length(occurred_boot_ns)=20 AND occurred_boot_ns NOT GLOB '*[^0-9]*' AND occurred_boot_ns<='18446744073709551615'),
 event_kind TEXT NOT NULL,
 result_code TEXT NOT NULL,
 detail_json TEXT NOT NULL CHECK(length(detail_json)<=4096)
);
-- Records of refused/prepare/cancelled actions without an intent yet; never assume every denied row has an intent FK.
CREATE TABLE IF NOT EXISTS action_attempts (
 request_id TEXT PRIMARY KEY,
 payload_sha256 TEXT NOT NULL CHECK(length(payload_sha256)=64),
 session_digest TEXT NOT NULL,
 occurred_utc TEXT NOT NULL,
 result_code TEXT NOT NULL,
 detail_json TEXT NOT NULL CHECK(length(detail_json)<=4096)
);
CREATE INDEX IF NOT EXISTS idx_action_events_time ON action_events(occurred_utc);
CREATE INDEX IF NOT EXISTS idx_action_intents_time ON action_intents(created_utc);
CREATE INDEX IF NOT EXISTS idx_action_attempts_time ON action_attempts(occurred_utc);
PRAGMA user_version = 2;
COMMIT;
