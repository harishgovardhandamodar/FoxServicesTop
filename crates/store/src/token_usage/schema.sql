-- fox-services-top token usage log: one row per poll per (node, app,
-- model) with the windowed totals the node reported, for the consumption
-- history. fox-services-top's own version 1, format 1. Rows older than the
-- configured retention are pruned on append.
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
BEGIN IMMEDIATE;

-- Windowed token flow of one poll: cumulative within the node's window,
-- so a timeseries over `utc_ns` shows consumption, not deltas.
CREATE TABLE IF NOT EXISTS token_samples (
    utc_ns INTEGER NOT NULL,
    node TEXT NOT NULL,
    app TEXT NOT NULL,
    model TEXT NOT NULL,
    requests INTEGER NOT NULL CHECK (requests >= 0),
    ingress_tokens INTEGER NOT NULL CHECK (ingress_tokens >= 0),
    egress_tokens INTEGER NOT NULL CHECK (egress_tokens >= 0),
    PRIMARY KEY (utc_ns, node, app, model)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS token_samples_time ON token_samples (utc_ns);

PRAGMA user_version = 1;
COMMIT;
