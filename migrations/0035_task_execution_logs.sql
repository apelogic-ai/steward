-- Retain explicitly enabled, bounded Task execution transcripts after runtime cleanup.
-- Logs are attempt evidence, not declared Task outputs, and are written atomically with
-- the terminal adapter observation.
ALTER TABLE task_execution_attempts
    ADD COLUMN execution_stdout bytea,
    ADD COLUMN execution_stderr bytea,
    ADD CONSTRAINT task_execution_logs_are_paired CHECK (
        (execution_stdout IS NULL) = (execution_stderr IS NULL)
    ),
    ADD CONSTRAINT task_execution_stdout_is_bounded CHECK (
        execution_stdout IS NULL OR octet_length(execution_stdout) <= 4194304
    ),
    ADD CONSTRAINT task_execution_stderr_is_bounded CHECK (
        execution_stderr IS NULL OR octet_length(execution_stderr) <= 4194304
    ),
    ADD CONSTRAINT task_execution_logs_are_terminal CHECK (
        execution_stdout IS NULL OR state IN ('succeeded', 'failed')
    );

COMMENT ON COLUMN task_execution_attempts.execution_stdout IS
    'Bounded stdout captured only when full execution-log diagnostics were explicitly requested.';

COMMENT ON COLUMN task_execution_attempts.execution_stderr IS
    'Bounded stderr captured only when full execution-log diagnostics were explicitly requested.';
