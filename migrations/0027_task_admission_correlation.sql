-- Task admission recovery must use a durable server-authored identity rather than
-- reconstructing ownership from mutable-looking runtime and principal fields.
-- NULL preserves non-Task decisions and historical rows. The existing
-- admission_decisions append-only trigger makes a populated correlation immutable.
ALTER TABLE admission_decisions
    ADD COLUMN task_uid uuid REFERENCES task_submissions(task_uid);

CREATE UNIQUE INDEX admission_decisions_task_uid_unique
    ON admission_decisions (task_uid)
    WHERE task_uid IS NOT NULL;

COMMENT ON COLUMN admission_decisions.task_uid IS
    'Immutable server-authored Task correlation; NULL for non-Task decisions and historical rows.';
