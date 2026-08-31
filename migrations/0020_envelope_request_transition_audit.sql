-- Every envelope-request lifecycle transition must carry the authenticated
-- principal and the exact immutable template revision that governed it.
-- Older events predate that contract, so mark their actor honestly rather
-- than inventing a user or administrator identity.
ALTER TABLE envelope_request_events
    ADD COLUMN actor text,
    ADD COLUMN template_revision bigint;

-- Rolling deployments leave old apiserver pods alive after this migration
-- commits. Complete their legacy inserts at the database boundary until a
-- later release can remove this compatibility trigger.
CREATE FUNCTION steward_fill_envelope_request_event_audit()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.actor IS NULL THEN
        NEW.actor := 'steward-system/legacy-writer';
    END IF;
    IF NEW.template_revision IS NULL THEN
        SELECT requests.template_revision
        INTO NEW.template_revision
        FROM envelope_requests requests
        WHERE requests.id = NEW.request_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER envelope_request_events_fill_legacy_audit
BEFORE INSERT ON envelope_request_events
FOR EACH ROW EXECUTE FUNCTION steward_fill_envelope_request_event_audit();

-- This is the one controlled history rewrite: the trigger is restored before
-- the transaction commits, so no externally visible state loses append-only
-- enforcement.
ALTER TABLE envelope_request_events
    DISABLE TRIGGER envelope_request_events_are_append_only;

UPDATE envelope_request_events events
SET actor = 'steward-system/legacy-migration',
    template_revision = requests.template_revision
FROM envelope_requests requests
WHERE requests.id = events.request_id;

ALTER TABLE envelope_request_events
    ENABLE TRIGGER envelope_request_events_are_append_only;

ALTER TABLE envelope_request_events
    ALTER COLUMN actor SET NOT NULL,
    ALTER COLUMN template_revision SET NOT NULL,
    ADD CONSTRAINT envelope_request_events_actor_nonempty CHECK (actor <> ''),
    ADD CONSTRAINT envelope_request_events_template_revision_positive
        CHECK (template_revision > 0);
