-- Steward v0.2 removes Service Envelope authority from Task orchestration.
--
-- Unfinished v0.1.23 Tasks are resumable only when their immutable authority can
-- be recovered exactly. Historical terminal rows remain readable as version 1/2.
LOCK TABLE task_submissions IN SHARE ROW EXCLUSIVE MODE;

ALTER TABLE task_submissions
    ADD COLUMN authority_kind text,
    ADD COLUMN user_envelope_snapshot jsonb,
    ADD CONSTRAINT task_submissions_authority_kind_shape CHECK (
        authority_kind IS NULL OR authority_kind IN ('user-envelope', 'internal')
    ),
    ADD CONSTRAINT task_submissions_user_envelope_snapshot_shape CHECK (
        user_envelope_snapshot IS NULL
        OR jsonb_typeof(user_envelope_snapshot) = 'object'
    );

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM task_submissions
        WHERE NOT finalized
          AND orchestration_version = 2
          AND NOT (
              (
                  user_envelope_instance_id IS NOT NULL
                  AND user_envelope_revision IS NOT NULL
                  AND user_envelope_digest IS NOT NULL
                  AND internal_authority_id IS NULL
                  AND internal_authority_version IS NULL
                  AND internal_authority_digest IS NULL
              )
              OR
              (
                  user_envelope_instance_id IS NULL
                  AND user_envelope_revision IS NULL
                  AND user_envelope_digest IS NULL
                  AND internal_authority_id IS NOT NULL
                  AND internal_authority_version IS NOT NULL
                  AND internal_authority_digest IS NOT NULL
              )
          )
    ) THEN
        RAISE EXCEPTION
            'v0.2 upgrade refuses unfinished Tasks without one complete immutable authority';
    END IF;
END
$$;

-- The old writer fence and immutable-intent trigger must be replaced before the
-- one-time v2-to-v3 backfill. The migration is transactional, so a failed
-- backfill restores both automatically.
ALTER TABLE task_submissions
    DROP CONSTRAINT task_submissions_new_orchestration_version_required,
    DROP CONSTRAINT task_submissions_durable_intent_complete;

DROP TRIGGER durable_task_intent_is_immutable ON task_submissions;

ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_durable_intent_complete CHECK (
        orchestration_version = 1
        OR (
            orchestration_version = 2
            AND candidate_digest IS NOT NULL
            AND orchestration_operation_id IS NOT NULL
            AND service_envelope_digest IS NOT NULL
            AND original_admission_decision IS NOT NULL
            AND original_admission_deltas IS NOT NULL
        )
        OR (
            orchestration_version = 3
            AND candidate_digest IS NOT NULL
            AND orchestration_operation_id IS NOT NULL
            AND envelope_revision IS NULL
            AND service_envelope_digest IS NULL
            AND original_admission_decision IS NOT NULL
            AND original_admission_deltas IS NOT NULL
        )
    );

UPDATE task_submissions tasks
SET authority_kind = 'user-envelope',
    user_envelope_snapshot = (
        SELECT events.approved_envelope
        FROM envelope_requests requests
        JOIN envelope_request_events events ON events.request_id = requests.id
        WHERE requests.owner_user_id = tasks.owner_user_id
          AND events.status = 'provisioned'
          AND events.envelope_instance_id = tasks.user_envelope_instance_id
          AND events.envelope_digest = tasks.user_envelope_digest
          AND events.approved_envelope ->> 'revision'
              = tasks.user_envelope_revision::text
        ORDER BY events.id DESC
        LIMIT 1
    ),
    orchestration_version = 3,
    envelope_revision = NULL,
    service_envelope_digest = NULL
WHERE NOT tasks.finalized
  AND tasks.orchestration_version = 2
  AND tasks.user_envelope_instance_id IS NOT NULL;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM task_submissions
        WHERE NOT finalized
          AND orchestration_version = 3
          AND authority_kind = 'user-envelope'
          AND user_envelope_snapshot IS NULL
    ) THEN
        RAISE EXCEPTION
            'v0.2 upgrade cannot recover the exact approved User Envelope snapshot for an unfinished Task';
    END IF;
END
$$;

UPDATE task_submissions
SET authority_kind = 'internal',
    orchestration_version = 3,
    envelope_revision = NULL,
    service_envelope_digest = NULL
WHERE NOT finalized
  AND orchestration_version = 2
  AND internal_authority_id IS NOT NULL;

ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_v3_authority_complete CHECK (
        (
            orchestration_version IN (1, 2)
            AND authority_kind IS NULL
            AND user_envelope_snapshot IS NULL
        )
        OR (
            orchestration_version = 3
            AND original_admission_decision = 'admit'
            AND original_admission_deltas = '[]'::jsonb
            AND (
                (
                    authority_kind = 'user-envelope'
                    AND user_envelope_instance_id IS NOT NULL
                    AND user_envelope_revision IS NOT NULL
                    AND user_envelope_digest IS NOT NULL
                    AND user_envelope_snapshot IS NOT NULL
                    AND user_envelope_snapshot ->> 'revision'
                        = user_envelope_revision::text
                    AND internal_authority_id IS NULL
                    AND internal_authority_version IS NULL
                    AND internal_authority_digest IS NULL
                )
                OR
                (
                    authority_kind = 'internal'
                    AND user_envelope_instance_id IS NULL
                    AND user_envelope_revision IS NULL
                    AND user_envelope_digest IS NULL
                    AND user_envelope_snapshot IS NULL
                    AND internal_authority_id IS NOT NULL
                    AND internal_authority_version IS NOT NULL
                    AND internal_authority_digest IS NOT NULL
                )
            )
        )
    ),
    ADD CONSTRAINT task_submissions_new_orchestration_version_required
        CHECK (orchestration_version = 3)
        NOT VALID;

CREATE OR REPLACE FUNCTION steward_reject_durable_task_intent_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.orchestration_version IN (2, 3) AND (
        NEW.task_uid IS DISTINCT FROM OLD.task_uid
        OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key
        OR NEW.submitter_service IS DISTINCT FROM OLD.submitter_service
        OR NEW.acting_user IS DISTINCT FROM OLD.acting_user
        OR NEW.acting_user_id IS DISTINCT FROM OLD.acting_user_id
        OR NEW.owner IS DISTINCT FROM OLD.owner
        OR NEW.owner_user_id IS DISTINCT FROM OLD.owner_user_id
        OR NEW.identity_binding_state IS DISTINCT FROM OLD.identity_binding_state
        OR NEW.workflow IS DISTINCT FROM OLD.workflow
        OR NEW.workflow_name IS DISTINCT FROM OLD.workflow_name
        OR NEW.workflow_version IS DISTINCT FROM OLD.workflow_version
        OR NEW.workflow_digest IS DISTINCT FROM OLD.workflow_digest
        OR NEW.user_envelope_instance_id IS DISTINCT FROM OLD.user_envelope_instance_id
        OR NEW.user_envelope_revision IS DISTINCT FROM OLD.user_envelope_revision
        OR NEW.user_envelope_digest IS DISTINCT FROM OLD.user_envelope_digest
        OR NEW.user_envelope_snapshot IS DISTINCT FROM OLD.user_envelope_snapshot
        OR NEW.internal_authority_id IS DISTINCT FROM OLD.internal_authority_id
        OR NEW.internal_authority_version IS DISTINCT FROM OLD.internal_authority_version
        OR NEW.internal_authority_digest IS DISTINCT FROM OLD.internal_authority_digest
        OR NEW.authority_kind IS DISTINCT FROM OLD.authority_kind
        OR NEW.coding_agent_runtime IS DISTINCT FROM OLD.coding_agent_runtime
        OR NEW.runtime_uid IS DISTINCT FROM OLD.runtime_uid
        OR NEW.runtime_namespace IS DISTINCT FROM OLD.runtime_namespace
        OR NEW.runtime_name IS DISTINCT FROM OLD.runtime_name
        OR NEW.runtime_ownership IS DISTINCT FROM OLD.runtime_ownership
        OR NEW.runtime_spec IS DISTINCT FROM OLD.runtime_spec
        OR NEW.agent_command IS DISTINCT FROM OLD.agent_command
        OR NEW.execution_binding IS DISTINCT FROM OLD.execution_binding
        OR NEW.direct_task_evidence IS DISTINCT FROM OLD.direct_task_evidence
        OR NEW.envelope_revision IS DISTINCT FROM OLD.envelope_revision
        OR NEW.orchestration_version IS DISTINCT FROM OLD.orchestration_version
        OR NEW.orchestration_operation_id IS DISTINCT FROM OLD.orchestration_operation_id
        OR NEW.candidate_digest IS DISTINCT FROM OLD.candidate_digest
        OR NEW.service_envelope_digest IS DISTINCT FROM OLD.service_envelope_digest
        OR NEW.original_admission_decision IS DISTINCT FROM OLD.original_admission_decision
        OR NEW.original_admission_deltas IS DISTINCT FROM OLD.original_admission_deltas
    ) THEN
        RAISE EXCEPTION 'durable Task intent is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER durable_task_intent_is_immutable
BEFORE UPDATE ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_durable_task_intent_mutation();

CREATE OR REPLACE FUNCTION steward_validate_task_commands_are_monotonic()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    connection_result_consumed boolean := false;
    internal_output_retirement boolean := false;
BEGIN
    IF NEW.output_archive IS DISTINCT FROM OLD.output_archive
        AND OLD.orchestration_version IN (2, 3)
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

    IF OLD.orchestration_version IN (2, 3)
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

ALTER TABLE task_runtime_operations
    DROP CONSTRAINT task_runtime_operations_activation_authority_kind_check,
    ADD CONSTRAINT task_runtime_operations_activation_authority_kind_check CHECK (
        activation_authority_kind IN ('baseline', 'grant', 'internal', 'user-envelope')
    );

COMMENT ON COLUMN task_submissions.authority_kind IS
    'Immutable v3 Task authority: exact provisioned User Envelope or code-owned internal authority.';

COMMENT ON COLUMN task_submissions.user_envelope_snapshot IS
    'Exact immutable approved User Envelope used for v3 admission and recovery; never reconstructed from Service Envelope state.';

COMMENT ON COLUMN task_submissions.orchestration_version IS
    'Staged-rollout fence: versions 1/2 are historical; every new Task uses User-Envelope-only orchestration version 3.';
