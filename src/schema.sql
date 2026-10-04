CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS owner (
    id INTEGER PRIMARY KEY CHECK (id=1),
    username TEXT NOT NULL,
    password_hash TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (token_hash TEXT PRIMARY KEY, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY,
    date TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('expense','income','transfer','refund')),
    amount_cents INTEGER NOT NULL CHECK(amount_cents>=0 AND (kind!='refund' OR amount_cents>0)),
    source TEXT NOT NULL,
    external_id TEXT,
    payload TEXT NOT NULL,
    UNIQUE(source, external_id)
);
CREATE INDEX IF NOT EXISTS entries_date ON entries(date);
CREATE TABLE IF NOT EXISTS rules (
    id INTEGER PRIMARY KEY,
    pattern TEXT NOT NULL COLLATE NOCASE,
    description_pattern TEXT NOT NULL DEFAULT '' COLLATE NOCASE,
    category TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('expense','income','transfer','refund')),
    UNIQUE(kind,pattern,description_pattern)
);
CREATE TABLE IF NOT EXISTS previews (token TEXT PRIMARY KEY, created INTEGER NOT NULL, payload TEXT NOT NULL);
