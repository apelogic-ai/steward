-- Deployment execution details are nullable for rows created before the binding catalog.
-- New catalog-backed Tasks persist the complete immutable binding at reservation time.
ALTER TABLE task_submissions
    ADD COLUMN execution_binding jsonb,
    ADD CONSTRAINT task_submissions_execution_binding_object
        CHECK (
            execution_binding IS NULL
            OR jsonb_typeof(execution_binding) = 'object'
        );

COMMENT ON COLUMN task_submissions.execution_binding IS
    'Immutable server-resolved disposable or resident execution binding; NULL identifies legacy default-image Tasks.';
