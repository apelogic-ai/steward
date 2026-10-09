-- Managed inference credentials are one encrypted value per canonical user. The
-- key encryption key remains deployment configuration and never enters Postgres.
CREATE TABLE managed_inference_credentials (
    user_id text PRIMARY KEY REFERENCES canonical_users(user_id) ON DELETE RESTRICT,
    ciphertext bytea NOT NULL CHECK (octet_length(ciphertext) BETWEEN 17 AND 8208),
    credential_nonce bytea NOT NULL CHECK (octet_length(credential_nonce) = 12),
    wrapped_data_key bytea NOT NULL CHECK (octet_length(wrapped_data_key) = 48),
    wrapping_nonce bytea NOT NULL CHECK (octet_length(wrapping_nonce) = 12),
    key_version integer NOT NULL CHECK (key_version = 1),
    last_four text NOT NULL CHECK (last_four ~ '^[!-~]{4}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE managed_inference_credential_audit (
    id uuid PRIMARY KEY,
    user_id text NOT NULL CHECK (btrim(user_id) <> ''),
    action text NOT NULL CHECK (action IN ('added', 'replaced', 'removed')),
    actor text NOT NULL CHECK (btrim(actor) <> ''),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX managed_inference_credential_audit_by_user
ON managed_inference_credential_audit (user_id, at DESC, id DESC);

CREATE TRIGGER managed_inference_credential_audit_is_immutable
BEFORE UPDATE OR DELETE ON managed_inference_credential_audit
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

COMMENT ON TABLE managed_inference_credentials IS
    'One envelope-encrypted managed LiteLLM credential per canonical Steward user.';
COMMENT ON TABLE managed_inference_credential_audit IS
    'Append-only add, replace, and remove history; this table never contains credential material.';
