CREATE TABLE grants (
    id uuid PRIMARY KEY,
    runtime_uid text NOT NULL,
    dimension text NOT NULL CHECK (dimension IN ('budget', 'ttl', 'models', 'tools')),
    granted_value jsonb NOT NULL,
    approval_id uuid NOT NULL REFERENCES approvals(id),
    expires_at timestamptz,
    at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (approval_id, dimension)
);

CREATE INDEX grants_by_runtime_uid
ON grants (runtime_uid, at);

CREATE TRIGGER grants_are_append_only
BEFORE UPDATE OR DELETE ON grants
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
