-- Personal agents are owner-scoped durable definitions. Turns and messages form the
-- single-writer inbox consumed by the apiserver dispatcher.
CREATE TABLE personal_agents (
    id uuid PRIMARY KEY,
    owner_user_id text NOT NULL REFERENCES canonical_users(user_id) ON DELETE RESTRICT,
    name text NOT NULL CHECK (name <> '' AND char_length(name) <= 200),
    agent_ref text NOT NULL CHECK (agent_ref <> '' AND char_length(agent_ref) <= 255),
    instructions text NOT NULL CHECK (octet_length(instructions) <= 16384),
    envelope_request_id uuid NOT NULL REFERENCES envelope_requests(id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX personal_agents_by_owner
    ON personal_agents (owner_user_id, created_at, id);

CREATE TABLE personal_agent_turns (
    id uuid PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES personal_agents(id) ON DELETE CASCADE,
    status text NOT NULL CHECK (status IN ('queued', 'running', 'done', 'failed')),
    task_uid uuid REFERENCES task_submissions(task_uid) ON DELETE RESTRICT,
    reply text CHECK (reply IS NULL OR octet_length(reply) <= 1048576),
    failure_reason text CHECK (
        failure_reason IS NULL
        OR (failure_reason <> '' AND char_length(failure_reason) <= 500)
    ),
    lease_owner text,
    lease_expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    completed_at timestamptz,
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK (
        (status = 'queued' AND started_at IS NULL AND completed_at IS NULL)
        OR (status = 'running' AND started_at IS NOT NULL AND completed_at IS NULL)
        OR (status IN ('done', 'failed') AND started_at IS NOT NULL AND completed_at IS NOT NULL)
    ),
    CHECK ((status = 'done') = (reply IS NOT NULL)),
    CHECK ((status = 'failed') = (failure_reason IS NOT NULL))
);

CREATE UNIQUE INDEX personal_agent_one_queued_turn
    ON personal_agent_turns (agent_id) WHERE status = 'queued';
CREATE UNIQUE INDEX personal_agent_one_running_turn
    ON personal_agent_turns (agent_id) WHERE status = 'running';
CREATE INDEX personal_agent_turns_thread
    ON personal_agent_turns (agent_id, created_at, id);
CREATE INDEX personal_agent_turns_dispatch
    ON personal_agent_turns (status, lease_expires_at, created_at, id);

CREATE TABLE personal_agent_messages (
    id uuid PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES personal_agents(id) ON DELETE CASCADE,
    turn_id uuid NOT NULL REFERENCES personal_agent_turns(id) ON DELETE CASCADE,
    idempotency_key text NOT NULL
        CHECK (idempotency_key <> '' AND char_length(idempotency_key) <= 255),
    content text NOT NULL CHECK (content <> '' AND octet_length(content) <= 16384),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (agent_id, idempotency_key)
);

CREATE INDEX personal_agent_messages_by_turn
    ON personal_agent_messages (turn_id, created_at, id);

CREATE TRIGGER personal_agent_messages_are_immutable
BEFORE UPDATE OR DELETE ON personal_agent_messages
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

-- TODO(#587): define bounded retention and an expired-resume contract before pruning.
CREATE TABLE personal_agent_turn_events (
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES personal_agents(id) ON DELETE CASCADE,
    turn_id uuid NOT NULL REFERENCES personal_agent_turns(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind = 'turn.updated'),
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX personal_agent_turn_events_resume
    ON personal_agent_turn_events (agent_id, seq);

CREATE TRIGGER personal_agent_turn_events_are_append_only
BEFORE UPDATE OR DELETE ON personal_agent_turn_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

CREATE FUNCTION steward_notify_personal_agent_turn_event()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_notify('steward_personal_agent_turn_events', NEW.seq::text);
    RETURN NEW;
END;
$$;

CREATE TRIGGER personal_agent_turn_event_notify
AFTER INSERT ON personal_agent_turn_events
FOR EACH ROW EXECUTE FUNCTION steward_notify_personal_agent_turn_event();
