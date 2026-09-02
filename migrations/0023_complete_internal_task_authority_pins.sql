ALTER TABLE task_submissions
    DROP CONSTRAINT task_submissions_internal_authority_pins_complete,
    ADD CONSTRAINT task_submissions_internal_authority_pins_complete CHECK (
        (internal_authority_id IS NULL
            AND internal_authority_version IS NULL
            AND internal_authority_digest IS NULL)
        OR
        (internal_authority_id IS NOT NULL
            AND internal_authority_version IS NOT NULL
            AND internal_authority_digest IS NOT NULL
            AND btrim(internal_authority_id) <> ''
            AND internal_authority_version > 0
            AND internal_authority_digest ~ '^sha256:[0-9a-f]{64}$')
    );
