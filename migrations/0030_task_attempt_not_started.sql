-- A claim is not evidence that execution was authorized. Fence a never-authorized
-- attempt before cleanup without inventing an external start or acknowledgement.
ALTER TABLE task_execution_attempts
    DROP CONSTRAINT task_execution_attempts_state_check,
    DROP CONSTRAINT task_execution_attempts_check,
    DROP CONSTRAINT task_execution_attempts_check1,
    ADD CONSTRAINT task_execution_attempts_state_check CHECK (state IN (
        'start_pending', 'running', 'succeeded', 'failed', 'cancel_pending',
        'outcome_unknown', 'not_started'
    )),
    ADD CONSTRAINT task_execution_attempts_terminal_finished CHECK (
        state NOT IN ('succeeded', 'failed', 'outcome_unknown', 'not_started')
        OR finished_at IS NOT NULL
    ),
    ADD CONSTRAINT task_execution_attempts_start_authorized CHECK (
        state IN ('start_pending', 'not_started') OR start_invoked_at IS NOT NULL
    ),
    ADD CONSTRAINT task_execution_attempts_not_started_evidence CHECK (
        state <> 'not_started' OR (
            start_invoked_at IS NULL AND started_at IS NULL
            AND adapter_observation_id IS NULL AND result_digest IS NULL
            AND result_reference IS NULL AND last_error_code IS NOT NULL
        )
    );

CREATE OR REPLACE FUNCTION steward_validate_task_execution_attempt_transition()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.attempt_id IS DISTINCT FROM OLD.attempt_id
        OR NEW.task_uid IS DISTINCT FROM OLD.task_uid
        OR NEW.operation_id IS DISTINCT FROM OLD.operation_id
        OR NEW.runtime_uid IS DISTINCT FROM OLD.runtime_uid
        OR NEW.active_manifest_digest IS DISTINCT FROM OLD.active_manifest_digest
        OR NEW.command_digest IS DISTINCT FROM OLD.command_digest
        OR NEW.input_digest IS DISTINCT FROM OLD.input_digest
    THEN
        RAISE EXCEPTION 'Task execution attempt identity is immutable'
            USING ERRCODE = '55000';
    END IF;
    IF OLD.adapter_observation_id IS NOT NULL
        AND NEW.adapter_observation_id IS DISTINCT FROM OLD.adapter_observation_id
    THEN
        RAISE EXCEPTION 'Task execution adapter observation identity is immutable'
            USING ERRCODE = '55000';
    END IF;
    IF OLD.result_digest IS NOT NULL
        AND (NEW.result_digest IS DISTINCT FROM OLD.result_digest
             OR NEW.result_reference IS DISTINCT FROM OLD.result_reference)
    THEN
        RAISE EXCEPTION 'Task execution result identity is immutable'
            USING ERRCODE = '55000';
    END IF;
    IF OLD.start_invoked_at IS NOT NULL
        AND (NEW.start_invoked_at IS DISTINCT FROM OLD.start_invoked_at
             OR NEW.start_observation_deadline_at
                IS DISTINCT FROM OLD.start_observation_deadline_at)
    THEN
        RAISE EXCEPTION 'Task execution start intent is immutable'
            USING ERRCODE = '55000';
    END IF;
    IF NEW.generation <> OLD.generation + 1 THEN
        RAISE EXCEPTION 'Task execution attempt generation must advance exactly once'
            USING ERRCODE = '55000';
    END IF;
    IF OLD.state IN ('succeeded', 'failed', 'outcome_unknown', 'not_started') THEN
        RAISE EXCEPTION 'terminal Task execution attempt is immutable'
            USING ERRCODE = '55000';
    END IF;
    IF NOT (
        NEW.state = OLD.state
        OR (OLD.state = 'start_pending' AND NEW.state IN ('running', 'succeeded', 'failed', 'cancel_pending', 'outcome_unknown', 'not_started'))
        OR (OLD.state = 'running' AND NEW.state IN ('succeeded', 'failed', 'cancel_pending', 'outcome_unknown'))
        OR (OLD.state = 'cancel_pending' AND NEW.state IN ('succeeded', 'failed', 'outcome_unknown'))
    ) THEN
        RAISE EXCEPTION 'invalid Task execution attempt state transition: % -> %',
            OLD.state, NEW.state USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;
