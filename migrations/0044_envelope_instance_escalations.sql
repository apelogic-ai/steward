-- Cumulative-limit decisions are instance-scoped, append-only authority. They must never
-- rewrite the published template or the originally approved envelope request.
CREATE TABLE envelope_instance_grants (
    id uuid PRIMARY KEY,
    escalation_id bigint NOT NULL UNIQUE REFERENCES inference_exhaustions(id),
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    dimension text NOT NULL CHECK (dimension IN ('llm_spend', 'runtime_minutes')),
    amount numeric NOT NULL CHECK (amount > 0),
    base_limit numeric NOT NULL CHECK (base_limit >= 0),
    target_limit numeric NOT NULL,
    unit text NOT NULL CHECK (unit IN ('USD', 'min')),
    valid_until timestamptz NOT NULL,
    rationale text NOT NULL CHECK (rationale <> ''),
    granted_by text NOT NULL CHECK (granted_by <> ''),
    at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT envelope_instance_grants_exact_target
        CHECK (target_limit = base_limit + amount),
    CONSTRAINT envelope_instance_grants_future_expiry
        CHECK (valid_until > at)
);

CREATE INDEX envelope_instance_grants_by_instance
ON envelope_instance_grants (envelope_instance_id, at DESC);

CREATE TRIGGER envelope_instance_grants_are_append_only
BEFORE UPDATE OR DELETE ON envelope_instance_grants
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE cumulative_escalation_denials (
    escalation_id bigint PRIMARY KEY REFERENCES inference_exhaustions(id),
    envelope_instance_id text NOT NULL CHECK (envelope_instance_id <> ''),
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    rationale text NOT NULL CHECK (rationale <> ''),
    denied_by text NOT NULL CHECK (denied_by <> ''),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER cumulative_escalation_denials_are_append_only
BEFORE UPDATE OR DELETE ON cumulative_escalation_denials
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
