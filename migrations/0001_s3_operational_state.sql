CREATE TABLE envelopes (
    scope_kind text NOT NULL CHECK (scope_kind = 'member_role'),
    scope_ref text NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    spec jsonb NOT NULL,
    authored_by text NOT NULL,
    at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (scope_kind, scope_ref, revision)
);

CREATE TABLE admission_decisions (
    id uuid PRIMARY KEY,
    runtime_uid text NOT NULL,
    spec_digest text NOT NULL,
    envelope_rev bigint NOT NULL CHECK (envelope_rev > 0),
    verdict text NOT NULL CHECK (verdict IN ('admit', 'reject')),
    deltas jsonb NOT NULL,
    proposed_spec jsonb NOT NULL,
    actor text NOT NULL,
    member_role text NOT NULL,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE approvals (
    id uuid PRIMARY KEY,
    runtime_uid text NOT NULL,
    admission_decision_id uuid NOT NULL REFERENCES admission_decisions(id),
    state text NOT NULL CHECK (state IN ('pending', 'approved', 'rejected')),
    jira_key text,
    decided_by text,
    decided_at timestamptz,
    rationale text,
    evidence_url text,
    CHECK (
        (state = 'pending' AND decided_by IS NULL AND decided_at IS NULL)
        OR
        (state <> 'pending' AND decided_by IS NOT NULL AND decided_at IS NOT NULL)
    )
);

CREATE TABLE runtime_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    runtime_uid text NOT NULL,
    phase_from text,
    phase_to text NOT NULL,
    actor text NOT NULL,
    reason text NOT NULL,
    payload jsonb NOT NULL,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION steward_reject_history_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER envelopes_are_immutable
BEFORE UPDATE OR DELETE ON envelopes
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TRIGGER admission_decisions_are_append_only
BEFORE UPDATE OR DELETE ON admission_decisions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TRIGGER runtime_events_are_append_only
BEFORE UPDATE OR DELETE ON runtime_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
