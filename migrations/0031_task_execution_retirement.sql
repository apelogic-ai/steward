-- Execution outcome is immutable history, not proof that a runtime is reusable.
-- Keep current ownership separate from append-only retirement evidence.
CREATE TABLE task_execution_retirements (
    attempt_id uuid PRIMARY KEY REFERENCES task_execution_attempts(attempt_id),
    runtime_uid text NOT NULL CHECK (runtime_uid <> ''),
    evidence_kind text NOT NULL CHECK (evidence_kind IN ('never_authorized', 'adapter_terminal')),
    adapter_observation_id text,
    observed_at timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (evidence_kind = 'never_authorized' AND adapter_observation_id IS NULL)
        OR (evidence_kind = 'adapter_terminal' AND adapter_observation_id IS NOT NULL
            AND adapter_observation_id <> '')
    )
);

CREATE TABLE task_runtime_execution_leases (
    runtime_uid text PRIMARY KEY CHECK (runtime_uid <> ''),
    attempt_id uuid NOT NULL UNIQUE REFERENCES task_execution_attempts(attempt_id)
        DEFERRABLE INITIALLY DEFERRED
);

-- Existing successful/failed observations have durable adapter terminal evidence.
-- Unknown outcomes deliberately get no retirement, including finalized Tasks.
INSERT INTO task_execution_retirements
    (attempt_id, runtime_uid, evidence_kind, adapter_observation_id, observed_at)
SELECT attempt_id, runtime_uid,
       CASE WHEN state = 'not_started' THEN 'never_authorized' ELSE 'adapter_terminal' END,
       adapter_observation_id, finished_at
FROM task_execution_attempts WHERE state IN ('succeeded', 'failed', 'not_started');

-- Fail closed on pre-existing overlapping unknown executions. Migration must not
-- silently choose an owner or manufacture retirement evidence to resolve them.
INSERT INTO task_runtime_execution_leases (runtime_uid, attempt_id)
SELECT runtime_uid, attempt_id FROM task_execution_attempts
WHERE state IN ('start_pending', 'running', 'cancel_pending', 'outcome_unknown');

CREATE FUNCTION steward_validate_execution_retirement()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE attempt task_execution_attempts%ROWTYPE;
BEGIN
    SELECT * INTO STRICT attempt FROM task_execution_attempts WHERE attempt_id = NEW.attempt_id;
    IF NEW.runtime_uid <> attempt.runtime_uid
        OR (NEW.evidence_kind = 'never_authorized'
            AND (attempt.state <> 'not_started' OR attempt.start_invoked_at IS NOT NULL))
        OR (NEW.evidence_kind = 'adapter_terminal'
            AND (attempt.state NOT IN ('succeeded', 'failed', 'outcome_unknown')
                 OR attempt.start_invoked_at IS NULL
                 OR (attempt.adapter_observation_id IS NOT NULL
                     AND attempt.adapter_observation_id <> NEW.adapter_observation_id)))
    THEN
        RAISE EXCEPTION 'execution retirement requires exact attempt evidence' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER task_execution_retirement_requires_evidence
BEFORE INSERT ON task_execution_retirements
FOR EACH ROW EXECUTE FUNCTION steward_validate_execution_retirement();
CREATE TRIGGER task_execution_retirement_is_immutable
BEFORE UPDATE OR DELETE ON task_execution_retirements
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE FUNCTION steward_validate_execution_lease_release()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM task_execution_retirements
                   WHERE attempt_id = OLD.attempt_id AND runtime_uid = OLD.runtime_uid) THEN
        RAISE EXCEPTION 'execution ownership cannot be released without retirement evidence'
            USING ERRCODE = '55000';
    END IF;
    RETURN OLD;
END;
$$;
CREATE TRIGGER task_runtime_execution_lease_release_requires_evidence
BEFORE DELETE ON task_runtime_execution_leases
FOR EACH ROW EXECUTE FUNCTION steward_validate_execution_lease_release();
CREATE TRIGGER task_runtime_execution_lease_identity_is_immutable
BEFORE UPDATE ON task_runtime_execution_leases
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE FUNCTION steward_validate_execution_ownership()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE attempt task_execution_attempts%ROWTYPE;
BEGIN
    SELECT * INTO STRICT attempt FROM task_execution_attempts WHERE attempt_id = NEW.attempt_id;
    IF NOT EXISTS (SELECT 1 FROM task_execution_retirements
                   WHERE attempt_id = attempt.attempt_id AND runtime_uid = attempt.runtime_uid)
       AND NOT EXISTS (SELECT 1 FROM task_runtime_execution_leases
                       WHERE attempt_id = attempt.attempt_id AND runtime_uid = attempt.runtime_uid)
    THEN
        RAISE EXCEPTION 'every unretired execution requires its exact runtime lease'
            USING ERRCODE = '55000';
    END IF;
    IF TG_TABLE_NAME = 'task_runtime_execution_leases' AND NEW.runtime_uid <> attempt.runtime_uid THEN
        RAISE EXCEPTION 'execution lease must match exact runtime UID' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER task_execution_attempt_requires_ownership
AFTER INSERT ON task_execution_attempts DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION steward_validate_execution_ownership();
CREATE CONSTRAINT TRIGGER task_runtime_execution_lease_requires_identity
AFTER INSERT ON task_runtime_execution_leases DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION steward_validate_execution_ownership();

-- The explicit lease supersedes the terminal-state predicate, including for
-- outcome_unknown. Retain only the stronger ownership boundary.
DROP INDEX task_execution_attempts_one_active_per_runtime;
