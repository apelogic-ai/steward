ALTER TABLE task_submissions
ADD COLUMN envelope_revision bigint CHECK (envelope_revision > 0);

CREATE TABLE task_lifecycle_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    task_uid uuid NOT NULL REFERENCES task_submissions(task_uid),
    event_kind text NOT NULL
        CHECK (event_kind IN ('phase', 'finalization_requested', 'finalized')),
    phase text
        CHECK (phase IN ('submitted', 'parked', 'queued', 'running', 'succeeded', 'failed', 'cancelled')),
    provenance text NOT NULL CHECK (provenance IN ('recorded', 'backfilled')),
    at timestamptz NOT NULL,
    CHECK (
        (event_kind = 'phase' AND phase IS NOT NULL)
        OR
        (event_kind <> 'phase' AND phase IS NULL)
    )
);

CREATE INDEX task_lifecycle_events_by_task
ON task_lifecycle_events (task_uid, at, id);

CREATE TRIGGER task_lifecycle_events_are_append_only
BEFORE UPDATE OR DELETE ON task_lifecycle_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

INSERT INTO task_lifecycle_events (task_uid, event_kind, phase, provenance, at)
SELECT task_uid, 'phase', 'submitted', 'backfilled', created_at
FROM task_submissions;

INSERT INTO task_lifecycle_events (task_uid, event_kind, phase, provenance, at)
SELECT task_uid, 'phase', phase, 'backfilled', updated_at
FROM task_submissions
WHERE phase <> 'submitted';

INSERT INTO task_lifecycle_events (task_uid, event_kind, provenance, at)
SELECT task_uid, 'finalization_requested', 'backfilled', updated_at
FROM task_submissions
WHERE finalize_requested;

INSERT INTO task_lifecycle_events (task_uid, event_kind, provenance, at)
SELECT task_uid, 'finalized', 'backfilled', updated_at
FROM task_submissions
WHERE finalized;

CREATE FUNCTION steward_record_task_insert_lifecycle()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO task_lifecycle_events (task_uid, event_kind, phase, provenance, at)
    VALUES (NEW.task_uid, 'phase', NEW.phase, 'recorded', NEW.created_at);
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_insert_records_lifecycle
AFTER INSERT ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_record_task_insert_lifecycle();

CREATE FUNCTION steward_record_task_update_lifecycle()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.phase IS DISTINCT FROM OLD.phase THEN
        INSERT INTO task_lifecycle_events (task_uid, event_kind, phase, provenance, at)
        VALUES (NEW.task_uid, 'phase', NEW.phase, 'recorded', NEW.updated_at);
    END IF;
    IF NEW.finalize_requested AND NOT OLD.finalize_requested THEN
        INSERT INTO task_lifecycle_events (task_uid, event_kind, provenance, at)
        VALUES (NEW.task_uid, 'finalization_requested', 'recorded', NEW.updated_at);
    END IF;
    IF NEW.finalized AND NOT OLD.finalized THEN
        INSERT INTO task_lifecycle_events (task_uid, event_kind, provenance, at)
        VALUES (NEW.task_uid, 'finalized', 'recorded', NEW.updated_at);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_update_records_lifecycle
AFTER UPDATE OF phase, finalize_requested, finalized ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_record_task_update_lifecycle();
