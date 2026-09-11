-- Direct-package Tasks bind exact source, closure, authority, and diagnostics before
-- reservation. Existing legacy and catalog Tasks retain NULL evidence.
ALTER TABLE task_submissions
    ADD COLUMN direct_task_evidence jsonb,
    ADD CONSTRAINT task_submissions_direct_evidence_object CHECK (
        direct_task_evidence IS NULL
        OR ((
            jsonb_typeof(direct_task_evidence) = 'object'
            AND direct_task_evidence ->> 'schemaVersion'
                = 'steward.task/source-authority-evidence/v1'
            AND direct_task_evidence ->> 'taskUid' = task_uid::text
            AND jsonb_typeof(direct_task_evidence -> 'sourceProvenance') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'invocation') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'package') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'closure') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'envelope') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'effectiveRequirements') = 'object'
            AND jsonb_typeof(direct_task_evidence -> 'diagnostics') = 'object'
            AND direct_task_evidence #>> '{envelope,revision}'
                = user_envelope_revision::text
            AND direct_task_evidence #>> '{envelope,digest}'
                = 'steward:' || user_envelope_digest
        ) IS TRUE)
    );

-- Direct packages pin an exact User Envelope but do not acquire a legacy catalog
-- Workflow identity. Preserve both older legal pin sets and require a complete direct set.
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
    );

CREATE FUNCTION steward_reject_direct_task_evidence_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.direct_task_evidence IS DISTINCT FROM OLD.direct_task_evidence THEN
        RAISE EXCEPTION 'direct Task binding evidence is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER direct_task_binding_evidence_is_immutable
BEFORE UPDATE OF direct_task_evidence ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_direct_task_evidence_mutation();

COMMENT ON COLUMN task_submissions.direct_task_evidence IS
    'Immutable server-authored direct Git package source, authority, closure, and diagnostics evidence.';
