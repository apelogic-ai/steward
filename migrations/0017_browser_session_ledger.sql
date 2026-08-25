-- Browser sessions are opaque, revocable local state.  The raw HttpOnly cookie
-- value is never persisted; only its SHA-256 digest is an authorization key.
CREATE TABLE browser_sessions (
    token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id text NOT NULL REFERENCES canonical_users(user_id),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX browser_sessions_active_by_expiry
    ON browser_sessions (expires_at);

CREATE INDEX browser_sessions_active_by_user
    ON browser_sessions (user_id, expires_at);
