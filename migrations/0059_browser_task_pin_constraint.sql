-- Browser-authored direct packages have immutable browser evidence and an exact
-- User Envelope snapshot, but intentionally do not acquire a legacy Workflow
-- identity. Preserve the two pre-existing pin shapes and admit that third shape.
ALTER TABLE task_submissions
    DROP CONSTRAINT task_submissions_versioned_pins_complete,
    ADD CONSTRAINT task_submissions_versioned_pins_complete CHECK (
        (direct_task_evidence IS NULL AND (
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
        ))
        OR
        (direct_task_evidence IS NOT NULL
            AND workflow_name IS NULL
            AND workflow_version IS NULL
            AND workflow_digest IS NULL
            AND user_envelope_instance_id IS NOT NULL
            AND user_envelope_revision IS NOT NULL
            AND user_envelope_digest IS NOT NULL
            AND btrim(user_envelope_instance_id) <> ''
            AND user_envelope_revision > 0
            AND user_envelope_digest ~ '^sha256:[0-9a-f]{64}$')
        OR
        (task_origin = 'browser'
            AND browser_task_evidence IS NOT NULL
            AND direct_task_evidence IS NULL
            AND workflow_name IS NULL
            AND workflow_version IS NULL
            AND workflow_digest IS NULL
            AND user_envelope_instance_id IS NOT NULL
            AND user_envelope_revision IS NOT NULL
            AND user_envelope_digest IS NOT NULL
            AND btrim(user_envelope_instance_id) <> ''
            AND user_envelope_revision > 0
            AND user_envelope_digest ~ '^sha256:[0-9a-f]{64}$')
    );
