-- PostgreSQL CHECK constraints accept an UNKNOWN expression. Require every
-- versioned pin explicitly so a partially populated set cannot pass because a
-- nullable comparison evaluated to NULL.
ALTER TABLE task_submissions
    DROP CONSTRAINT task_submissions_versioned_pins_complete,
    ADD CONSTRAINT task_submissions_versioned_pins_complete
        CHECK (
            (workflow_name IS NULL
                AND workflow_version IS NULL
                AND workflow_digest IS NULL
                AND user_envelope_instance_id IS NULL
                AND user_envelope_revision IS NULL
                AND user_envelope_digest IS NULL)
            OR
            (workflow_name IS NOT NULL
                AND workflow_version IS NOT NULL
                AND workflow_digest IS NOT NULL
                AND user_envelope_instance_id IS NOT NULL
                AND user_envelope_revision IS NOT NULL
                AND user_envelope_digest IS NOT NULL
                AND workflow_name ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'
                AND workflow_version > 0
                AND workflow_digest ~ '^sha256:[0-9a-f]{64}$'
                AND btrim(user_envelope_instance_id) <> ''
                AND user_envelope_revision > 0
                AND user_envelope_digest ~ '^sha256:[0-9a-f]{64}$')
        );
