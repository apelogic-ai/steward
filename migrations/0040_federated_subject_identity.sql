-- Additive task-auth identity observation. Existing canonical identities, Tasks, runs,
-- Envelopes and authority bindings are neither updated nor backfilled.
CREATE TABLE federated_subjects (
    subject_id uuid PRIMARY KEY,
    issuer text NOT NULL
        CHECK (
            issuer <> ''
            AND issuer = btrim(issuer)
            AND length(issuer) <= 2048
        ),
    subject text NOT NULL
        CHECK (
            subject <> ''
            AND length(subject) <= 255
            AND subject !~ '[[:space:]]'
        ),
    state text NOT NULL DEFAULT 'observed'
        CHECK (state IN ('observed', 'associated', 'disabled')),
    canonical_user_id text REFERENCES canonical_users(user_id),
    actor_login text
        CHECK (
            actor_login IS NULL
            OR (
                actor_login <> ''
                AND actor_login = btrim(actor_login)
                AND length(actor_login) <= 128
            )
        ),
    display_name text
        CHECK (
            display_name IS NULL
            OR (
                display_name <> ''
                AND display_name = btrim(display_name)
                AND length(display_name) <= 256
            )
        ),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT federated_subjects_external_identity_unique UNIQUE (issuer, subject),
    CONSTRAINT federated_subjects_state_binding CHECK (
        (state = 'observed' AND canonical_user_id IS NULL)
        OR (state = 'associated' AND canonical_user_id IS NOT NULL)
        OR state = 'disabled'
    )
);

CREATE INDEX federated_subjects_current_state
    ON federated_subjects (state, last_seen_at DESC, subject_id);

CREATE TABLE federated_subject_audit (
    event_id uuid PRIMARY KEY,
    subject_id uuid NOT NULL
        REFERENCES federated_subjects(subject_id),
    action text NOT NULL
        CHECK (action IN ('observed', 'v2_seeded', 'associated', 'replaced', 'disabled')),
    actor text NOT NULL
        CHECK (actor <> '' AND actor = btrim(actor) AND length(actor) <= 255),
    previous_canonical_user_id text REFERENCES canonical_users(user_id),
    canonical_user_id text REFERENCES canonical_users(user_id),
    previous_revision bigint NOT NULL CHECK (previous_revision >= 0),
    revision bigint NOT NULL CHECK (revision > 0 AND revision >= previous_revision),
    reason text
        CHECK (
            reason IS NULL
            OR (reason <> '' AND reason = btrim(reason) AND length(reason) <= 2000)
        ),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT federated_subject_audit_revision_unique UNIQUE (subject_id, revision),
    CONSTRAINT federated_subject_audit_observation_shape CHECK (
        action <> 'observed'
        OR (
            previous_revision = 0
            AND revision = 1
            AND previous_canonical_user_id IS NULL
            AND canonical_user_id IS NULL
        )
    )
);

CREATE INDEX federated_subject_audit_history
    ON federated_subject_audit (subject_id, revision, created_at, event_id);

CREATE TRIGGER federated_subject_audit_is_append_only
BEFORE UPDATE OR DELETE ON federated_subject_audit
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

COMMENT ON TABLE federated_subjects IS
    'Observed task-auth identities keyed only by exact trusted issuer and authenticated subject; observation grants no Task or Envelope authority.';

COMMENT ON COLUMN federated_subjects.canonical_user_id IS
    'Explicit or v2-proven association to Steward canonical identity; never inferred from login, display name, or email.';

COMMENT ON TABLE federated_subject_audit IS
    'Append-only administrator and verified-v2 transition history for federated-subject associations.';
