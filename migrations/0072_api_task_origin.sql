-- Non-browser callers without GitHub source provenance use an explicit API origin.
-- Historical rows retain their existing immutable origin; this widens only the accepted set.
ALTER TABLE task_submissions
    DROP CONSTRAINT task_submissions_task_origin_valid,
    ADD CONSTRAINT task_submissions_task_origin_valid CHECK (
        task_origin IN ('browser', 'github-actions', 'connections', 'api', 'unknown')
    );

COMMENT ON COLUMN task_submissions.task_origin IS
    'Immutable authoring origin: browser, github-actions, connections, api, or unknown historical origin.';
