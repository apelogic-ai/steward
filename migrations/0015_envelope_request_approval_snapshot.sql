-- An approval is a distinct, append-only authority snapshot. It must never be
-- reconstructed from browser state or assumed equal to the original request.
-- Existing lifecycle events remain valid and explicitly surface as no snapshot.
ALTER TABLE envelope_request_events
    ADD COLUMN approved_envelope jsonb;

CREATE INDEX envelope_request_events_approved_snapshot
    ON envelope_request_events (request_id, at DESC, id DESC)
    WHERE approved_envelope IS NOT NULL;
