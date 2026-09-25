-- GHA-style stages use explicit append-only facts. These are written in the same transaction as
-- the task/runtime transition rather than reconstructed from timestamps or log availability.
CREATE TABLE task_stage_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    event_kind text NOT NULL CHECK (event_kind IN (
        'admitted', 'runtime_bound', 'execution_started', 'execution_ended'
    )),
    details jsonb NOT NULL DEFAULT '{}'::jsonb,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX task_stage_events_by_task ON task_stage_events (task_uid, at, id);

CREATE TRIGGER task_stage_events_are_append_only
BEFORE UPDATE OR DELETE ON task_stage_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

INSERT INTO task_stage_events (task_uid, event_kind, details, at)
SELECT task_uid, 'admitted', jsonb_build_object(
    'envelopeRevision', COALESCE(user_envelope_revision, envelope_revision),
    'envelopeDigest', user_envelope_digest
), created_at
FROM task_submissions;

INSERT INTO task_stage_events (task_uid, event_kind, details, at)
SELECT tasks.task_uid, 'runtime_bound', jsonb_build_object(
    'runtimeUid', COALESCE(operations.runtime_uid, tasks.runtime_uid),
    'ownership', tasks.runtime_ownership
), COALESCE(operations.observed_at, tasks.updated_at)
FROM task_submissions tasks
LEFT JOIN task_runtime_operations operations ON operations.task_uid = tasks.task_uid
WHERE COALESCE(operations.runtime_uid, tasks.runtime_uid) IS NOT NULL;

INSERT INTO task_stage_events (task_uid, event_kind, details, at)
SELECT task_uid, 'execution_started', '{}'::jsonb, at
FROM task_lifecycle_events WHERE event_kind = 'phase' AND phase = 'running';

INSERT INTO task_stage_events (task_uid, event_kind, details, at)
SELECT task_uid, 'execution_ended', jsonb_build_object('exitCategory', phase), at
FROM task_lifecycle_events
WHERE event_kind = 'phase' AND phase IN ('succeeded', 'failed', 'cancelled');

CREATE FUNCTION steward_record_task_admitted_stage()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO task_stage_events (task_uid, event_kind, details, at)
    VALUES (NEW.task_uid, 'admitted', jsonb_build_object(
        'envelopeRevision', COALESCE(NEW.user_envelope_revision, NEW.envelope_revision),
        'envelopeDigest', NEW.user_envelope_digest
    ), NEW.created_at);
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_insert_records_admitted_stage
AFTER INSERT ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_record_task_admitted_stage();

CREATE FUNCTION steward_record_task_execution_stage()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.phase = 'running' AND OLD.phase IS DISTINCT FROM NEW.phase THEN
        INSERT INTO task_stage_events (task_uid, event_kind, details, at)
        VALUES (NEW.task_uid, 'execution_started', '{}'::jsonb, NEW.updated_at);
    END IF;
    IF NEW.phase IN ('succeeded', 'failed', 'cancelled')
       AND OLD.phase IS DISTINCT FROM NEW.phase THEN
        INSERT INTO task_stage_events (task_uid, event_kind, details, at)
        VALUES (NEW.task_uid, 'execution_ended',
            jsonb_build_object('exitCategory', NEW.phase), NEW.updated_at);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_update_records_execution_stage
AFTER UPDATE OF phase ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_record_task_execution_stage();

CREATE FUNCTION steward_record_runtime_bound_stage()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.runtime_uid IS NULL THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'INSERT' THEN
        INSERT INTO task_stage_events (task_uid, event_kind, details, at)
        VALUES (NEW.task_uid, 'runtime_bound', jsonb_build_object(
            'runtimeUid', NEW.runtime_uid,
            'ownership', NEW.runtime_ownership
        ), COALESCE(NEW.observed_at, now()));
    ELSIF OLD.runtime_uid IS DISTINCT FROM NEW.runtime_uid THEN
        INSERT INTO task_stage_events (task_uid, event_kind, details, at)
        VALUES (NEW.task_uid, 'runtime_bound', jsonb_build_object(
            'runtimeUid', NEW.runtime_uid,
            'ownership', NEW.runtime_ownership
        ), COALESCE(NEW.observed_at, now()));
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_runtime_operation_records_bound_stage
AFTER INSERT OR UPDATE ON task_runtime_operations
FOR EACH ROW EXECUTE FUNCTION steward_record_runtime_bound_stage();
