-- External Task caller context is display-only. The authenticated submitter_service remains the
-- service identity; historical Tasks retain NULL without a backfill.
ALTER TABLE task_submissions
    ADD COLUMN caller_reference jsonb,
    ADD CONSTRAINT task_submissions_caller_reference_shape CHECK (
        caller_reference IS NULL OR (
            task_origin = 'api'
            AND jsonb_typeof(caller_reference) = 'object'
            AND caller_reference ? 'label'
            AND (caller_reference - ARRAY['label', 'url']) = '{}'::jsonb
            AND jsonb_typeof(caller_reference -> 'label') = 'string'
            AND octet_length(caller_reference ->> 'label') BETWEEN 1 AND 128
            AND btrim(caller_reference ->> 'label') = caller_reference ->> 'label'
            AND caller_reference ->> 'label' ~ '^[A-Za-z0-9 ._#:/@+()·-]+$'
            AND (
                NOT (caller_reference ? 'url') OR (
                    jsonb_typeof(caller_reference -> 'url') = 'string'
                    AND octet_length(caller_reference ->> 'url') BETWEEN 1 AND 2048
                    AND caller_reference ->> 'url' LIKE 'https://%'
                    AND caller_reference ->> 'url' !~ '[[:space:][:cntrl:]\\]'
                    AND split_part(split_part(split_part(substring(caller_reference ->> 'url' FROM 9), '/', 1), '?', 1), '#', 1) ~ '[A-Za-z0-9]'
                    AND split_part(split_part(split_part(substring(caller_reference ->> 'url' FROM 9), '/', 1), '?', 1), '#', 1) !~ '@'
                )
            )
        )
    ) NOT VALID;

ALTER TABLE task_submissions
    VALIDATE CONSTRAINT task_submissions_caller_reference_shape;

CREATE FUNCTION steward_reject_task_caller_reference_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.caller_reference IS DISTINCT FROM OLD.caller_reference THEN
        RAISE EXCEPTION 'Task caller reference is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_caller_reference_is_immutable
BEFORE UPDATE OF caller_reference ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_task_caller_reference_mutation();

COMMENT ON COLUMN task_submissions.caller_reference IS
    'Optional immutable display-only label and HTTPS link from an authenticated external caller.';
