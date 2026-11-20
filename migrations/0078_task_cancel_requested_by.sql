-- Record which browser user asked to cancel a run: the owner, or a tenant administrator.
-- Historical Tasks and cancellations from other callers retain NULL without a backfill.
ALTER TABLE task_submissions
    ADD COLUMN cancel_requested_by text,
    ADD CONSTRAINT task_submissions_cancel_requested_by_requires_cancel CHECK (
        cancel_requested_by IS NULL OR cancel_requested
    ) NOT VALID;

ALTER TABLE task_submissions
    VALIDATE CONSTRAINT task_submissions_cancel_requested_by_requires_cancel;

CREATE FUNCTION steward_reject_task_cancel_requested_by_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.cancel_requested_by IS NOT NULL
        AND NEW.cancel_requested_by IS DISTINCT FROM OLD.cancel_requested_by
    THEN
        RAISE EXCEPTION 'Task cancellation requester is immutable once recorded'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_cancel_requested_by_is_immutable
BEFORE UPDATE OF cancel_requested_by ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_task_cancel_requested_by_mutation();

COMMENT ON COLUMN task_submissions.cancel_requested_by IS
    'Canonical user ID of the browser user who first requested cancellation; NULL for historical Tasks and other callers.';
