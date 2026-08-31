-- Every envelope-request lifecycle transition must carry the authenticated
-- principal and the exact immutable template revision that governed it.
-- Older events predate that contract, so mark their actor honestly rather
-- than inventing a user or administrator identity.
ALTER TABLE envelope_request_events
    ADD COLUMN actor text,
    ADD COLUMN template_revision bigint;

UPDATE envelope_request_events events
SET actor = 'steward-system/legacy-migration',
    template_revision = requests.template_revision
FROM envelope_requests requests
WHERE requests.id = events.request_id;

ALTER TABLE envelope_request_events
    ALTER COLUMN actor SET NOT NULL,
    ALTER COLUMN template_revision SET NOT NULL,
    ADD CONSTRAINT envelope_request_events_actor_nonempty CHECK (actor <> ''),
    ADD CONSTRAINT envelope_request_events_template_revision_positive
        CHECK (template_revision > 0);
