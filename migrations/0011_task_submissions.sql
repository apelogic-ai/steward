CREATE TABLE task_submissions (
    task_uid uuid PRIMARY KEY,
    idempotency_key text NOT NULL CHECK (idempotency_key <> ''),
    submitter_service text NOT NULL CHECK (submitter_service <> ''),
    acting_user text,
    owner text NOT NULL CHECK (owner <> ''),
    workflow text NOT NULL CHECK (workflow <> ''),
    coding_agent_runtime text NOT NULL CHECK (coding_agent_runtime <> ''),
    runtime_uid text,
    runtime_namespace text NOT NULL CHECK (runtime_namespace <> ''),
    runtime_name text NOT NULL CHECK (runtime_name <> ''),
    runtime_ownership text NOT NULL
        CHECK (runtime_ownership IN ('provisioned', 'adopted')),
    phase text NOT NULL
        CHECK (phase IN ('submitted', 'parked', 'queued', 'running', 'succeeded', 'failed', 'cancelled')),
    runtime_spec jsonb NOT NULL,
    agent_command jsonb NOT NULL CHECK (jsonb_typeof(agent_command) = 'array'),
    input_archive bytea,
    output_archive bytea,
    execute_requested boolean NOT NULL DEFAULT false,
    finalize_requested boolean NOT NULL DEFAULT false,
    finalized boolean NOT NULL DEFAULT false,
    failure_reason text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (acting_user IS NULL OR acting_user <> ''),
    CHECK (runtime_uid IS NULL OR runtime_uid <> ''),
    CHECK (failure_reason IS NULL OR failure_reason <> ''),
    CHECK (NOT finalized OR finalize_requested)
);

CREATE UNIQUE INDEX task_submissions_submitter_idempotency
    ON task_submissions (submitter_service, idempotency_key);

CREATE INDEX task_submissions_execution_queue
    ON task_submissions (execute_requested, phase, created_at)
    WHERE execute_requested AND phase IN ('submitted', 'parked', 'queued');
