-- Live transcripts are bounded mutable snapshots of the current execution attempt. Terminal
-- transcripts remain immutable outcome evidence in the existing execution_stdout/stderr fields.
ALTER TABLE task_execution_attempts
    ADD COLUMN live_execution_stdout bytea,
    ADD COLUMN live_execution_stderr bytea,
    ADD CONSTRAINT task_live_execution_logs_are_paired CHECK (
        (live_execution_stdout IS NULL) = (live_execution_stderr IS NULL)
    ),
    ADD CONSTRAINT task_live_execution_stdout_is_bounded CHECK (
        live_execution_stdout IS NULL OR octet_length(live_execution_stdout) <= 4194304
    ),
    ADD CONSTRAINT task_live_execution_stderr_is_bounded CHECK (
        live_execution_stderr IS NULL OR octet_length(live_execution_stderr) <= 4194304
    );

COMMENT ON COLUMN task_execution_attempts.live_execution_stdout IS
    'Latest bounded stdout snapshot while an explicitly diagnosed Task attempt is running.';

COMMENT ON COLUMN task_execution_attempts.live_execution_stderr IS
    'Latest bounded stderr snapshot while an explicitly diagnosed Task attempt is running.';
