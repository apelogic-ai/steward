-- Durable, single-owner orchestration for Task runtime, approval, execution, and cleanup.
--
-- This migration is an offline lifecycle boundary. It intentionally refuses to reinterpret
-- an in-flight legacy Task because there is no durable effect history from which to recover it.
LOCK TABLE task_submissions IN SHARE ROW EXCLUSIVE MODE;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM task_submissions
        WHERE NOT finalized
    ) THEN
        RAISE EXCEPTION
            'durable Task orchestration rollout requires every legacy Task to be finalized';
    END IF;
END
$$;

-- Version 1 identifies read-only historical rows. The NOT VALID constraint preserves that
-- history while rejecting every old writer that omits the version on a new or updated row.
ALTER TABLE task_submissions
    ADD COLUMN orchestration_version smallint NOT NULL DEFAULT 1,
    ADD COLUMN orchestration_operation_id uuid,
    ADD COLUMN candidate_digest text,
    ADD COLUMN service_envelope_digest text,
    ADD COLUMN original_admission_decision text,
    ADD COLUMN original_admission_deltas jsonb,
    ADD COLUMN cancel_requested boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT task_submissions_candidate_digest_shape CHECK (
        candidate_digest IS NULL OR candidate_digest ~ '^sha256:[0-9a-f]{64}$'
    ),
    ADD CONSTRAINT task_submissions_service_envelope_digest_shape CHECK (
        service_envelope_digest IS NULL
        OR service_envelope_digest ~ '^sha256:[0-9a-f]{64}$'
    ),
    ADD CONSTRAINT task_submissions_original_admission_decision_shape CHECK (
        original_admission_decision IS NULL
        OR original_admission_decision IN ('admit', 'reject')
    ),
    ADD CONSTRAINT task_submissions_original_admission_deltas_shape CHECK (
        original_admission_deltas IS NULL
        OR jsonb_typeof(original_admission_deltas) = 'array'
    ),
    ADD CONSTRAINT task_submissions_durable_intent_complete CHECK (
        orchestration_version = 1
        OR (
            candidate_digest IS NOT NULL
            AND orchestration_operation_id IS NOT NULL
            AND service_envelope_digest IS NOT NULL
            AND original_admission_decision IS NOT NULL
            AND original_admission_deltas IS NOT NULL
        )
    ),
    ADD CONSTRAINT task_submissions_new_orchestration_version_required
        CHECK (orchestration_version = 2)
        NOT VALID;

COMMENT ON COLUMN task_submissions.orchestration_version IS
    'Staged-rollout fence: version 1 is finalized history; every new Task uses durable orchestration version 2.';

COMMENT ON COLUMN task_submissions.original_admission_deltas IS
    'Immutable submission-time rejection deltas; runtime-bound approval materialization happens only after exact UID observation.';

CREATE FUNCTION steward_reject_durable_task_intent_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.orchestration_version = 2 AND (
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
        OR NEW.internal_authority_id IS DISTINCT FROM OLD.internal_authority_id
        OR NEW.internal_authority_version IS DISTINCT FROM OLD.internal_authority_version
        OR NEW.internal_authority_digest IS DISTINCT FROM OLD.internal_authority_digest
        OR NEW.coding_agent_runtime IS DISTINCT FROM OLD.coding_agent_runtime
        OR NEW.runtime_uid IS DISTINCT FROM OLD.runtime_uid
        OR NEW.runtime_namespace IS DISTINCT FROM OLD.runtime_namespace
        OR NEW.runtime_name IS DISTINCT FROM OLD.runtime_name
        OR NEW.runtime_ownership IS DISTINCT FROM OLD.runtime_ownership
        OR NEW.runtime_spec IS DISTINCT FROM OLD.runtime_spec
        OR NEW.agent_command IS DISTINCT FROM OLD.agent_command
        OR NEW.execution_binding IS DISTINCT FROM OLD.execution_binding
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

CREATE FUNCTION steward_validate_task_commands_are_monotonic()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (OLD.finalized AND NEW IS DISTINCT FROM OLD)
        OR (OLD.input_archive IS NOT NULL AND NEW.input_archive IS DISTINCT FROM OLD.input_archive)
        OR (OLD.output_archive IS NOT NULL AND NEW.output_archive IS DISTINCT FROM OLD.output_archive)
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
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_commands_are_monotonic
BEFORE UPDATE ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_validate_task_commands_are_monotonic();

CREATE TABLE task_runtime_operations (
    task_uid uuid PRIMARY KEY REFERENCES task_submissions(task_uid),
    operation_id uuid NOT NULL UNIQUE,
    state text NOT NULL CHECK (state IN (
        'intent_recorded',
        'runtime_create_pending',
        'runtime_observed',
        'approval_pending',
        'activation_pending',
        'active',
        'cleanup_pending',
        'finalized'
    )),
    generation bigint NOT NULL DEFAULT 1 CHECK (generation > 0),
    runtime_ownership text NOT NULL CHECK (runtime_ownership IN (
        'provisioned', 'adopted', 'resident'
    )),
    runtime_namespace text NOT NULL CHECK (runtime_namespace <> ''),
    runtime_name text NOT NULL CHECK (runtime_name <> ''),
    inert_manifest_digest text NOT NULL
        CHECK (inert_manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    active_manifest_digest text NOT NULL
        CHECK (active_manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    expected_runtime_uid text,
    runtime_uid text,
    runtime_resource_version text,
    activation_authority_kind text
        CHECK (activation_authority_kind IN ('baseline', 'grant', 'internal')),
    activation_envelope_revision bigint CHECK (activation_envelope_revision > 0),
    activation_envelope_digest text
        CHECK (activation_envelope_digest ~ '^sha256:[0-9a-f]{64}$'),
    approval_id uuid REFERENCES approvals(id),
    retry_at timestamptz,
    last_error_code text CHECK (last_error_code IS NULL OR last_error_code <> ''),
    lease_owner text CHECK (lease_owner IS NULL OR lease_owner <> ''),
    lease_expires_at timestamptz,
    requested_at timestamptz NOT NULL DEFAULT now(),
    runtime_create_authorized_at timestamptz,
    observed_at timestamptz,
    activation_effect_authorized_at timestamptz,
    activated_at timestamptz,
    cleanup_requested_at timestamptz,
    runtime_absent_observed_at timestamptz,
    projections_absent_observed_at timestamptz,
    finalized_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (expected_runtime_uid IS NULL OR expected_runtime_uid <> ''),
    CHECK (runtime_uid IS NULL OR runtime_uid <> ''),
    CHECK (
        (runtime_ownership = 'provisioned' AND expected_runtime_uid IS NULL)
        OR (runtime_ownership IN ('adopted', 'resident') AND expected_runtime_uid IS NOT NULL)
    ),
    CHECK (runtime_resource_version IS NULL OR runtime_resource_version <> ''),
    CHECK (
        runtime_uid IS NULL
        OR (runtime_resource_version IS NOT NULL AND observed_at IS NOT NULL)
    ),
    CHECK (runtime_resource_version IS NULL OR runtime_uid IS NOT NULL),
    CHECK (observed_at IS NULL OR runtime_uid IS NOT NULL),
    CHECK (approval_id IS NULL OR runtime_uid IS NOT NULL),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK (
        state NOT IN ('intent_recorded', 'runtime_create_pending')
        OR runtime_uid IS NULL
    ),
    CHECK (state <> 'runtime_create_pending' OR runtime_ownership = 'provisioned'),
    CHECK (state <> 'runtime_create_pending' OR runtime_create_authorized_at IS NOT NULL),
    CHECK (
        state NOT IN ('runtime_observed', 'approval_pending', 'activation_pending', 'active')
        OR (runtime_uid IS NOT NULL AND observed_at IS NOT NULL)
    ),
    CHECK (state <> 'approval_pending' OR approval_id IS NOT NULL),
    CHECK (
        state NOT IN ('activation_pending', 'active')
        OR (
            activation_authority_kind IS NOT NULL
            AND activation_envelope_revision IS NOT NULL
            AND activation_envelope_digest IS NOT NULL
        )
    ),
    CHECK (
        state <> 'active'
        OR (activation_effect_authorized_at IS NOT NULL AND activated_at IS NOT NULL)
    ),
    CHECK (state <> 'cleanup_pending' OR cleanup_requested_at IS NOT NULL),
    CHECK (
        state <> 'finalized'
        OR (
            finalized_at IS NOT NULL
            AND (
                (
                    runtime_uid IS NULL
                    AND runtime_create_authorized_at IS NULL
                )
                OR (
                    projections_absent_observed_at IS NOT NULL
                    AND (
                        runtime_ownership IN ('adopted', 'resident')
                        OR runtime_absent_observed_at IS NOT NULL
                    )
                )
            )
        )
    )
);

ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_orchestration_operation_fk
        FOREIGN KEY (orchestration_operation_id)
        REFERENCES task_runtime_operations(operation_id)
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE admission_decisions
    ADD COLUMN orchestration_operation_id uuid
        REFERENCES task_runtime_operations(operation_id);

COMMENT ON COLUMN admission_decisions.orchestration_operation_id IS
    'Exact durable Task orchestration identity; NULL only for pre-0028 non-Task approval history.';

CREATE INDEX task_runtime_operations_due_work
    ON task_runtime_operations (retry_at, requested_at, task_uid)
    WHERE state <> 'finalized';

CREATE INDEX task_runtime_operations_runtime_uid
    ON task_runtime_operations (runtime_uid)
    WHERE runtime_uid IS NOT NULL;

CREATE TABLE task_execution_attempts (
    attempt_id uuid PRIMARY KEY,
    task_uid uuid NOT NULL UNIQUE REFERENCES task_submissions(task_uid),
    operation_id uuid NOT NULL REFERENCES task_runtime_operations(operation_id),
    runtime_uid text NOT NULL CHECK (runtime_uid <> ''),
    active_manifest_digest text NOT NULL
        CHECK (active_manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    command_digest text NOT NULL CHECK (command_digest ~ '^sha256:[0-9a-f]{64}$'),
    input_digest text NOT NULL CHECK (input_digest ~ '^sha256:[0-9a-f]{64}$'),
    state text NOT NULL CHECK (state IN (
        'start_pending', 'running', 'succeeded', 'failed', 'cancel_pending', 'outcome_unknown'
    )),
    generation bigint NOT NULL DEFAULT 1 CHECK (generation > 0),
    adapter_observation_id text
        CHECK (adapter_observation_id IS NULL OR adapter_observation_id <> ''),
    result_digest text CHECK (result_digest ~ '^sha256:[0-9a-f]{64}$'),
    result_reference text CHECK (result_reference IS NULL OR result_reference <> ''),
    retry_at timestamptz,
    last_error_code text CHECK (last_error_code IS NULL OR last_error_code <> ''),
    start_invoked_at timestamptz,
    start_observation_deadline_at timestamptz,
    started_at timestamptz,
    finished_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (
        state NOT IN ('succeeded', 'failed', 'outcome_unknown')
        OR finished_at IS NOT NULL
    ),
    CHECK (
        state = 'start_pending'
        OR start_invoked_at IS NOT NULL
    ),
    CHECK (
        state <> 'running'
        OR (adapter_observation_id IS NOT NULL AND started_at IS NOT NULL)
    ),
    CHECK (
        state NOT IN ('succeeded', 'failed')
        OR (adapter_observation_id IS NOT NULL AND started_at IS NOT NULL)
    ),
    CHECK (state <> 'outcome_unknown' OR last_error_code IS NOT NULL),
    CHECK (
        state <> 'succeeded'
        OR (result_digest IS NOT NULL AND result_reference IS NOT NULL)
    ),
    CHECK (
        (start_invoked_at IS NULL) = (start_observation_deadline_at IS NULL)
    )
);

CREATE INDEX task_execution_attempts_due_work
    ON task_execution_attempts (retry_at, created_at, attempt_id)
    WHERE state IN ('start_pending', 'running', 'cancel_pending');

CREATE TABLE external_effect_outbox (
    id uuid PRIMARY KEY,
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    operation_id uuid NOT NULL REFERENCES task_runtime_operations(operation_id),
    approval_id uuid NOT NULL UNIQUE REFERENCES approvals(id),
    effect_kind text NOT NULL CHECK (effect_kind = 'approval_delivery'),
    idempotency_key text NOT NULL UNIQUE CHECK (idempotency_key <> ''),
    state text NOT NULL CHECK (state IN ('pending', 'claimed', 'delivered', 'failed')),
    generation bigint NOT NULL DEFAULT 1 CHECK (generation > 0),
    claimed_by text CHECK (claimed_by IS NULL OR claimed_by <> ''),
    claimed_until timestamptz,
    external_reference text CHECK (external_reference IS NULL OR external_reference <> ''),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    retry_at timestamptz,
    last_error_code text CHECK (last_error_code IS NULL OR last_error_code <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((claimed_by IS NULL) = (claimed_until IS NULL)),
    CHECK (state <> 'claimed' OR claimed_by IS NOT NULL),
    CHECK (
        state <> 'delivered'
        OR (external_reference IS NOT NULL AND delivered_at IS NOT NULL)
    )
);

CREATE INDEX external_effect_outbox_due_delivery
    ON external_effect_outbox (retry_at, created_at, id)
    WHERE state IN ('pending', 'claimed');

CREATE FUNCTION steward_validate_external_effect_outbox_transition()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
        OR NEW.task_uid IS DISTINCT FROM OLD.task_uid
        OR NEW.operation_id IS DISTINCT FROM OLD.operation_id
        OR NEW.approval_id IS DISTINCT FROM OLD.approval_id
        OR NEW.effect_kind IS DISTINCT FROM OLD.effect_kind
        OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key
    THEN
        RAISE EXCEPTION 'external effect identity is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.generation <> OLD.generation + 1 THEN
        RAISE EXCEPTION 'external effect generation must advance exactly once'
            USING ERRCODE = '55000';
    END IF;

    IF NOT (
        NEW.state = OLD.state
        OR (OLD.state = 'pending' AND NEW.state IN ('claimed', 'failed'))
        OR (OLD.state = 'claimed' AND NEW.state IN ('pending', 'delivered', 'failed'))
        OR (OLD.state = 'failed' AND NEW.state = 'pending')
    ) THEN
        RAISE EXCEPTION 'invalid external effect state transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '55000';
    END IF;

    IF OLD.state = 'delivered' THEN
        RAISE EXCEPTION 'delivered external effect is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER external_effect_outbox_transition_is_monotonic
BEFORE UPDATE ON external_effect_outbox
FOR EACH ROW EXECUTE FUNCTION steward_validate_external_effect_outbox_transition();

CREATE TRIGGER external_effect_outbox_cannot_be_deleted
BEFORE DELETE ON external_effect_outbox
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE task_orchestration_journal (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    operation_id uuid NOT NULL REFERENCES task_runtime_operations(operation_id),
    generation bigint NOT NULL CHECK (generation > 0),
    state text NOT NULL CHECK (state IN (
        'intent_recorded',
        'runtime_create_pending',
        'runtime_observed',
        'approval_pending',
        'activation_pending',
        'active',
        'cleanup_pending',
        'finalized'
    )),
    event_kind text NOT NULL CHECK (event_kind <> ''),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    actor text NOT NULL CHECK (actor <> ''),
    at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (operation_id, generation)
);

CREATE FUNCTION steward_validate_task_runtime_operation_transition()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.task_uid IS DISTINCT FROM OLD.task_uid
        OR NEW.operation_id IS DISTINCT FROM OLD.operation_id
        OR NEW.runtime_ownership IS DISTINCT FROM OLD.runtime_ownership
        OR NEW.runtime_namespace IS DISTINCT FROM OLD.runtime_namespace
        OR NEW.runtime_name IS DISTINCT FROM OLD.runtime_name
        OR NEW.inert_manifest_digest IS DISTINCT FROM OLD.inert_manifest_digest
        OR NEW.active_manifest_digest IS DISTINCT FROM OLD.active_manifest_digest
        OR NEW.expected_runtime_uid IS DISTINCT FROM OLD.expected_runtime_uid
    THEN
        RAISE EXCEPTION 'Task runtime operation identity is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.runtime_uid IS NOT NULL AND NEW.runtime_uid IS DISTINCT FROM OLD.runtime_uid THEN
        RAISE EXCEPTION 'Task runtime UID is immutable once observed'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.approval_id IS NOT NULL AND NEW.approval_id IS DISTINCT FROM OLD.approval_id THEN
        RAISE EXCEPTION 'Task approval identity is immutable once materialized'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.generation <> OLD.generation + 1 THEN
        RAISE EXCEPTION 'Task runtime operation generation must advance exactly once'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.state = 'finalized' THEN
        RAISE EXCEPTION 'finalized Task runtime operation is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.state = 'intent_recorded'
        AND NEW.state = 'runtime_observed'
        AND OLD.runtime_ownership = 'provisioned'
    THEN
        RAISE EXCEPTION 'provisioned Task runtime observation requires prior create intent'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.state = OLD.state THEN
        RETURN NEW;
    END IF;

    IF NEW.state = 'cleanup_pending' AND OLD.state <> 'finalized' THEN
        RETURN NEW;
    END IF;

    IF NOT (
        (OLD.state = 'intent_recorded' AND NEW.state IN ('runtime_create_pending', 'runtime_observed', 'finalized'))
        OR (OLD.state = 'runtime_create_pending' AND NEW.state = 'runtime_observed')
        OR (OLD.state = 'runtime_observed' AND NEW.state IN ('approval_pending', 'activation_pending'))
        OR (OLD.state = 'approval_pending' AND NEW.state = 'activation_pending')
        OR (OLD.state = 'activation_pending' AND NEW.state = 'active')
        OR (OLD.state = 'cleanup_pending' AND NEW.state = 'finalized')
    ) THEN
        RAISE EXCEPTION 'invalid Task runtime operation state transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '55000';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER task_runtime_operation_transition_is_monotonic
BEFORE UPDATE ON task_runtime_operations
FOR EACH ROW EXECUTE FUNCTION steward_validate_task_runtime_operation_transition();

CREATE TRIGGER task_runtime_operations_cannot_be_deleted
BEFORE DELETE ON task_runtime_operations
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE FUNCTION steward_validate_task_execution_attempt_transition()
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
        AND (
            NEW.start_invoked_at IS DISTINCT FROM OLD.start_invoked_at
            OR NEW.start_observation_deadline_at
                IS DISTINCT FROM OLD.start_observation_deadline_at
        )
    THEN
        RAISE EXCEPTION 'Task execution start intent is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF NEW.generation <> OLD.generation + 1 THEN
        RAISE EXCEPTION 'Task execution attempt generation must advance exactly once'
            USING ERRCODE = '55000';
    END IF;

    IF OLD.state IN ('succeeded', 'failed', 'outcome_unknown') THEN
        RAISE EXCEPTION 'terminal Task execution attempt is immutable'
            USING ERRCODE = '55000';
    END IF;

    IF NOT (
        NEW.state = OLD.state
        OR (OLD.state = 'start_pending' AND NEW.state IN ('running', 'succeeded', 'failed', 'cancel_pending', 'outcome_unknown'))
        OR (OLD.state = 'running' AND NEW.state IN ('succeeded', 'failed', 'cancel_pending', 'outcome_unknown'))
        OR (OLD.state = 'cancel_pending' AND NEW.state IN ('succeeded', 'failed', 'outcome_unknown'))
    ) THEN
        RAISE EXCEPTION 'invalid Task execution attempt state transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '55000';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER task_execution_attempt_transition_is_monotonic
BEFORE UPDATE ON task_execution_attempts
FOR EACH ROW EXECUTE FUNCTION steward_validate_task_execution_attempt_transition();

CREATE TRIGGER task_execution_attempts_cannot_be_deleted
BEFORE DELETE ON task_execution_attempts
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TRIGGER task_orchestration_journal_is_append_only
BEFORE UPDATE OR DELETE ON task_orchestration_journal
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
