ALTER TABLE task_submissions
    ADD COLUMN internal_authority_id text,
    ADD COLUMN internal_authority_version bigint,
    ADD COLUMN internal_authority_digest text,
    ADD CONSTRAINT task_submissions_internal_authority_pins_complete CHECK (
        (internal_authority_id IS NULL
            AND internal_authority_version IS NULL
            AND internal_authority_digest IS NULL)
        OR
        (btrim(internal_authority_id) <> ''
            AND internal_authority_version > 0
            AND internal_authority_digest ~ '^sha256:[0-9a-f]{64}$')
    ),
    ADD CONSTRAINT task_submissions_internal_authority_key UNIQUE (
        task_uid,
        internal_authority_id,
        internal_authority_version,
        internal_authority_digest
    );

UPDATE task_submissions tasks
SET internal_authority_id = operations.authority_id,
    internal_authority_version = operations.authority_version,
    internal_authority_digest = operations.authority_digest
FROM connection_operations operations
WHERE operations.task_uid = tasks.task_uid;

ALTER TABLE connection_operations
    ADD CONSTRAINT connection_operations_task_authority_fk FOREIGN KEY (
        task_uid,
        authority_id,
        authority_version,
        authority_digest
    ) REFERENCES task_submissions (
        task_uid,
        internal_authority_id,
        internal_authority_version,
        internal_authority_digest
    );

COMMENT ON COLUMN task_submissions.internal_authority_id IS
    'Server-authored immutable authority identity for internal tasks; NULL for ordinary user tasks.';

COMMENT ON COLUMN task_submissions.internal_authority_version IS
    'Server-authored immutable authority version for internal tasks; NULL for ordinary user tasks.';

COMMENT ON COLUMN task_submissions.internal_authority_digest IS
    'Server-authored immutable authority digest for internal tasks; NULL for ordinary user tasks.';
