CREATE TABLE codex_runtime (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    host_id TEXT NOT NULL
);

CREATE TABLE codex_credentials (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL UNIQUE,
    subject TEXT NOT NULL,
    email TEXT,
    status TEXT NOT NULL CHECK (status IN ('active', 'signed_in', 'revoking', 'disconnected')),
    secret_enc TEXT,
    paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    reset_at INTEGER,
    generation INTEGER NOT NULL DEFAULT 0,
    operation_id TEXT,
    operation_until INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX codex_credentials_by_user ON codex_credentials(user_id, created_at);

CREATE TABLE codex_login_attempts (
    state_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    attempt_enc TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX codex_login_expiry ON codex_login_attempts(expires_at);

ALTER TABLE users ADD COLUMN default_codex_credential_id TEXT REFERENCES codex_credentials(id) ON DELETE SET NULL;
ALTER TABLE workspace_members ADD COLUMN codex_credential_id TEXT REFERENCES codex_credentials(id) ON DELETE SET NULL;
