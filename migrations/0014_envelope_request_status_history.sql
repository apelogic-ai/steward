-- User-facing envelope requests are immutable facts. Their current user-visible
-- status is derived from the latest append-only event; it is never guessed by
-- a browser or overwritten in place.
CREATE TABLE envelope_requests (
    id uuid PRIMARY KEY,
    owner_user_id text NOT NULL REFERENCES canonical_users(user_id),
    template_id text NOT NULL CHECK (template_id <> ''),
    template_revision bigint NOT NULL CHECK (template_revision > 0),
    requested_envelope jsonb NOT NULL,
    idempotency_key text NOT NULL CHECK (idempotency_key <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_user_id, idempotency_key)
);

CREATE INDEX envelope_requests_by_owner
    ON envelope_requests (owner_user_id, created_at DESC, id DESC);

CREATE TRIGGER envelope_requests_are_immutable
BEFORE UPDATE OR DELETE ON envelope_requests
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE TABLE envelope_request_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    request_id uuid NOT NULL REFERENCES envelope_requests(id),
    status text NOT NULL CHECK (status IN (
        'pending', 'approved', 'rejected', 'provisioned', 'stale', 'conflict'
    )),
    approval_id uuid,
    envelope_instance_id text,
    envelope_digest text,
    reason text,
    at timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (status = 'provisioned'
            AND envelope_instance_id IS NOT NULL
            AND envelope_digest IS NOT NULL)
        OR
        (status <> 'provisioned'
            AND envelope_instance_id IS NULL
            AND envelope_digest IS NULL)
    ),
    CHECK (reason IS NULL OR reason <> '')
);

CREATE INDEX envelope_request_events_current_status
    ON envelope_request_events (request_id, at DESC, id DESC);

CREATE TRIGGER envelope_request_events_are_append_only
BEFORE UPDATE OR DELETE ON envelope_request_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
