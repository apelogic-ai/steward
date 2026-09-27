-- Runtime minutes are cumulative authority on a provisioned Envelope instance. Keep the
-- AgentRuntime manifest immutable: observations, escalations, grants, and denials live in the
-- append-only control-plane ledger.

ALTER TABLE inference_exhaustions ADD COLUMN public_id uuid;
UPDATE inference_exhaustions
SET public_id = md5('steward:inference-exhaustion:' || id::text)::uuid;
ALTER TABLE inference_exhaustions ALTER COLUMN public_id SET NOT NULL;
ALTER TABLE inference_exhaustions ADD CONSTRAINT inference_exhaustions_public_id_unique
    UNIQUE (public_id);

CREATE TABLE runtime_minute_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    runtime_uid text NOT NULL CHECK (runtime_uid <> ''),
    period_start timestamptz NOT NULL,
    period_end timestamptz NOT NULL,
    observed_seconds numeric NOT NULL CHECK (observed_seconds >= 0),
    base_limit_minutes numeric NOT NULL CHECK (base_limit_minutes >= 0),
    effective_limit_minutes numeric NOT NULL CHECK (effective_limit_minutes >= base_limit_minutes),
    exhausted boolean NOT NULL,
    at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (period_start < period_end)
);

CREATE INDEX runtime_minute_observations_by_instance
ON runtime_minute_observations (envelope_instance_id, at DESC, id DESC);

CREATE TRIGGER runtime_minute_observations_are_append_only
BEFORE UPDATE OR DELETE ON runtime_minute_observations
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE runtime_minute_exhaustions (
    id uuid PRIMARY KEY,
    observation_id bigint NOT NULL UNIQUE REFERENCES runtime_minute_observations(id),
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    runtime_uid text NOT NULL CHECK (runtime_uid <> ''),
    period_start timestamptz NOT NULL,
    period_end timestamptz NOT NULL,
    observed_minutes numeric NOT NULL CHECK (observed_minutes >= 0),
    base_limit_minutes numeric NOT NULL CHECK (base_limit_minutes >= 0),
    effective_limit_minutes numeric NOT NULL CHECK (effective_limit_minutes >= base_limit_minutes),
    at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (period_start < period_end)
);

CREATE INDEX runtime_minute_exhaustions_by_runtime
ON runtime_minute_exhaustions (runtime_uid, period_start, at DESC);

CREATE TRIGGER runtime_minute_exhaustions_are_append_only
BEFORE UPDATE OR DELETE ON runtime_minute_exhaustions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE runtime_minute_instance_grants (
    id uuid PRIMARY KEY,
    escalation_id uuid NOT NULL UNIQUE REFERENCES runtime_minute_exhaustions(id),
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    amount numeric NOT NULL CHECK (amount > 0),
    base_limit numeric NOT NULL CHECK (base_limit >= 0),
    target_limit numeric NOT NULL,
    valid_until timestamptz NOT NULL,
    rationale text NOT NULL CHECK (rationale <> ''),
    granted_by text NOT NULL CHECK (granted_by <> ''),
    at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT runtime_minute_instance_grants_exact_target
        CHECK (target_limit = base_limit + amount),
    CONSTRAINT runtime_minute_instance_grants_future_expiry
        CHECK (valid_until > at)
);

CREATE INDEX runtime_minute_instance_grants_by_instance
ON runtime_minute_instance_grants (envelope_instance_id, at DESC);

CREATE TRIGGER runtime_minute_instance_grants_are_append_only
BEFORE UPDATE OR DELETE ON runtime_minute_instance_grants
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE runtime_minute_escalation_denials (
    escalation_id uuid PRIMARY KEY REFERENCES runtime_minute_exhaustions(id),
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    rationale text NOT NULL CHECK (rationale <> ''),
    denied_by text NOT NULL CHECK (denied_by <> ''),
    at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER runtime_minute_escalation_denials_are_append_only
BEFORE UPDATE OR DELETE ON runtime_minute_escalation_denials
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
