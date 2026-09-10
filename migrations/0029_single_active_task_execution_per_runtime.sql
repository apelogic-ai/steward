CREATE UNIQUE INDEX task_execution_attempts_one_active_per_runtime
ON task_execution_attempts (runtime_uid)
WHERE state IN ('start_pending', 'running', 'cancel_pending');
