ALTER TABLE admission_decisions
ADD COLUMN base_spec jsonb;

ALTER TABLE admission_decisions
ADD COLUMN runtime_namespace text;

ALTER TABLE admission_decisions
ADD COLUMN runtime_name text;

ALTER TABLE admission_decisions
ADD CONSTRAINT admission_decisions_require_runtime_snapshot
CHECK (
    base_spec IS NOT NULL
    AND runtime_namespace IS NOT NULL
    AND runtime_namespace <> ''
    AND runtime_name IS NOT NULL
    AND runtime_name <> ''
) NOT VALID;

DROP INDEX admission_decisions_exact_request;

CREATE INDEX grants_by_runtime_authority
ON grants (runtime_uid, envelope_revision, expires_at);
