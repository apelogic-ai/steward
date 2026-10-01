-- Persist only source provenance ratified by authenticated Task identity resolution.
-- Existing direct-package evidence already contains the same immutable provenance and
-- is deterministically backfilled; other historical Tasks remain unknown.
ALTER TABLE task_submissions
    ADD COLUMN source_provenance jsonb;

UPDATE task_submissions
SET source_provenance = direct_task_evidence -> 'sourceProvenance'
WHERE direct_task_evidence IS NOT NULL;

ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_source_provenance_shape CHECK (
        source_provenance IS NULL
        OR ((
            jsonb_typeof(source_provenance) = 'object'
            AND source_provenance ->> 'contractVersion' = 'steward.source-provenance/v1'
            AND source_provenance ->> 'provider' = 'github'
            AND jsonb_typeof(source_provenance -> 'repository') = 'object'
            AND source_provenance #>> '{repository,id}' ~ '^[0-9]{1,20}$'
            AND source_provenance #>> '{repository,ownerId}' ~ '^[0-9]{1,20}$'
            AND length(source_provenance #>> '{repository,name}') BETWEEN 1 AND 512
            AND source_provenance ->> 'triggeredSha' ~ '^git:sha1:[0-9a-f]{40}$'
            AND jsonb_typeof(source_provenance -> 'run') = 'object'
            AND source_provenance #>> '{run,id}' ~ '^[0-9]{1,20}$'
            AND source_provenance #>> '{run,attempt}' ~ '^[1-9][0-9]*$'
            AND length(source_provenance ->> 'event') BETWEEN 1 AND 512
            AND length(source_provenance ->> 'ref') BETWEEN 1 AND 2048
            AND source_provenance ->> 'actorId' ~ '^[0-9]{1,20}$'
            AND length(source_provenance ->> 'actor') BETWEEN 1 AND 512
            AND jsonb_typeof(source_provenance -> 'callerWorkflow') = 'object'
            AND length(source_provenance #>> '{callerWorkflow,ref}') BETWEEN 1 AND 2048
            AND source_provenance #>> '{callerWorkflow,sha}' ~ '^git:sha1:[0-9a-f]{40}$'
            AND jsonb_typeof(source_provenance -> 'reusableWorkflow') = 'object'
            AND length(source_provenance #>> '{reusableWorkflow,ref}') BETWEEN 1 AND 2048
            AND source_provenance #>> '{reusableWorkflow,sha}' ~ '^git:sha1:[0-9a-f]{40}$'
        ) IS TRUE)
    ),
    ADD CONSTRAINT task_submissions_source_provenance_kind CHECK (
        source_provenance IS NULL
        OR direct_task_evidence IS NOT NULL
        OR workflow_name IS NOT NULL
    ),
    ADD CONSTRAINT task_submissions_direct_source_provenance_matches CHECK (
        direct_task_evidence IS NULL
        OR (source_provenance = direct_task_evidence -> 'sourceProvenance') IS TRUE
    );

CREATE FUNCTION steward_reject_task_source_provenance_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.source_provenance IS DISTINCT FROM OLD.source_provenance THEN
        RAISE EXCEPTION 'Task source provenance is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_source_provenance_is_immutable
BEFORE UPDATE OF source_provenance ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_task_source_provenance_mutation();

COMMENT ON COLUMN task_submissions.source_provenance IS
    'Immutable GitHub source provenance ratified by authenticated Task identity resolution; NULL is unknown and never inferred.';
