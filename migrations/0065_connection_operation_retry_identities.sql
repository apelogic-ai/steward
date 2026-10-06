DO $$
DECLARE
    legacy_constraint name;
BEGIN
    SELECT constraint_row.conname
      INTO STRICT legacy_constraint
      FROM pg_constraint AS constraint_row
     WHERE constraint_row.conrelid = 'connection_operations'::regclass
       AND constraint_row.contype = 'u'
       AND pg_get_constraintdef(constraint_row.oid)
           = 'UNIQUE (canonical_user_id, provider, idempotency_identity)';

    EXECUTE format(
        'ALTER TABLE connection_operations DROP CONSTRAINT %I',
        legacy_constraint
    );
END
$$;

ALTER TABLE connection_operations
ADD COLUMN publication_branch text
    CHECK (
        publication_branch IS NULL
        OR (
            operation_kind = 'publish'
            AND publication_branch ~ '^steward/task-[0-9a-f]{32}-[0-9a-f]{32}$'
        )
    );

CREATE INDEX connection_operations_by_idempotency_identity
ON connection_operations (
    canonical_user_id,
    provider,
    operation_kind,
    idempotency_identity,
    created_at DESC
);

COMMENT ON INDEX connection_operations_by_idempotency_identity IS
    'Supports bounded result reuse and client-key payload conflict checks while allowing a failed or expired governed operation to be retried as a new immutable row.';

COMMENT ON COLUMN connection_operations.publication_branch IS
    'Unpredictable server-selected branch capability retained across retries of one owner-scoped publication.';
