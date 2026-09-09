-- Internal connection responses may contain transient OAuth continuation data.
-- Retire only the consumed payload, never the immutable execution result identity.
ALTER TABLE task_submissions ADD COLUMN internal_output_retired_at timestamptz;

COMMENT ON COLUMN task_submissions.internal_output_retired_at IS
    'Database-owned, immutable evidence of consumed internal connection payload retirement; never permits restoring output.';

CREATE OR REPLACE FUNCTION steward_validate_task_commands_are_monotonic()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    connection_result_consumed boolean := false;
    internal_output_retirement boolean := false;
BEGIN
    IF NEW.output_archive IS DISTINCT FROM OLD.output_archive
        AND OLD.orchestration_version = 2
        AND OLD.internal_authority_id IS NOT NULL
    THEN
        SELECT EXISTS (
            SELECT 1 FROM connection_operations operation
            WHERE operation.task_uid = OLD.task_uid
                AND operation.operation_id = OLD.orchestration_operation_id
                AND operation.submitter_service = OLD.submitter_service
                AND operation.canonical_user_id = OLD.owner_user_id
                AND operation.authority_id = OLD.internal_authority_id
                AND operation.authority_version = OLD.internal_authority_version
                AND operation.authority_digest = OLD.internal_authority_digest
                AND operation.operation_state IN ('succeeded', 'failed')
        ) INTO connection_result_consumed;

        internal_output_retirement := connection_result_consumed
            AND OLD.internal_output_retired_at IS NULL
            AND OLD.output_archive IS NOT NULL
            AND NEW.output_archive IS NULL
            AND NEW.finalize_requested
            AND OLD.phase = 'succeeded'
            AND EXISTS (
                SELECT 1 FROM task_execution_attempts attempt
                JOIN task_runtime_operations operation
                    ON operation.task_uid = attempt.task_uid
                    AND operation.operation_id = attempt.operation_id
                    AND operation.runtime_uid = attempt.runtime_uid
                WHERE attempt.task_uid = OLD.task_uid
                    AND attempt.state = 'succeeded'
                    AND attempt.result_digest IS NOT NULL
                    AND attempt.result_reference IS NOT NULL
            );

        -- A consumed response cannot be replaced or restored after retirement.
        IF connection_result_consumed AND NOT internal_output_retirement THEN
            RAISE EXCEPTION 'consumed internal connection output can only be retired'
                USING ERRCODE = '55000';
        END IF;
    END IF;

    IF (OLD.finalized AND NEW IS DISTINCT FROM OLD)
        OR (NEW.internal_output_retired_at IS DISTINCT FROM OLD.internal_output_retired_at)
        OR (OLD.internal_output_retired_at IS NOT NULL AND NEW.output_archive IS NOT NULL)
        OR (OLD.input_archive IS NOT NULL AND NEW.input_archive IS DISTINCT FROM OLD.input_archive)
        OR (OLD.output_archive IS NOT NULL AND NEW.output_archive IS DISTINCT FROM OLD.output_archive
            AND NOT internal_output_retirement)
        OR (OLD.execute_requested AND NOT NEW.execute_requested)
        OR (OLD.cancel_requested AND NOT NEW.cancel_requested)
        OR (OLD.finalize_requested AND NOT NEW.finalize_requested)
        OR (OLD.finalized AND NOT NEW.finalized)
        OR (OLD.failure_reason IS NOT NULL AND NEW.failure_reason IS DISTINCT FROM OLD.failure_reason)
    THEN
        RAISE EXCEPTION 'durable Task commands and terminal observations are monotonic'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.orchestration_version = 2
        AND NEW.phase IS DISTINCT FROM OLD.phase
        AND NOT (
            (OLD.phase = 'submitted' AND NEW.phase IN ('queued', 'failed', 'cancelled'))
            OR (OLD.phase = 'parked' AND NEW.phase IN ('submitted', 'queued', 'failed', 'cancelled'))
            OR (OLD.phase = 'queued' AND NEW.phase IN ('running', 'succeeded', 'failed', 'cancelled'))
            OR (OLD.phase = 'running' AND NEW.phase IN ('succeeded', 'failed', 'cancelled'))
        )
    THEN
        RAISE EXCEPTION 'durable Task phase transition is not monotonic: % -> %',
            OLD.phase, NEW.phase
            USING ERRCODE = '55000';
    END IF;
    IF internal_output_retirement THEN
        NEW.internal_output_retired_at := clock_timestamp();
    END IF;
    RETURN NEW;
END;
$$;
