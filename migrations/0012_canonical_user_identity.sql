CREATE TABLE canonical_users (
    user_id text PRIMARY KEY
        CHECK (user_id ~ '^usr_[0-9a-f]{32}$'),
    organization_id text NOT NULL
        CHECK (organization_id ~ '^org_[a-z0-9_-]{1,60}$'),
    display_email text NOT NULL CHECK (display_email <> ''),
    state text NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'reconnect_required', 'disabled')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX canonical_users_one_display_email_per_organization
    ON canonical_users (organization_id, lower(display_email));

CREATE TABLE canonical_identity_subjects (
    issuer text NOT NULL CHECK (issuer <> ''),
    subject text NOT NULL CHECK (subject <> ''),
    organization_claim text NOT NULL CHECK (organization_claim <> ''),
    organization_id text NOT NULL
        CHECK (organization_id ~ '^org_[a-z0-9_-]{1,60}$'),
    user_id text NOT NULL REFERENCES canonical_users(user_id),
    verified_email text NOT NULL CHECK (verified_email <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject, organization_claim, organization_id),
    CONSTRAINT canonical_identity_subjects_external_pair_unique
        UNIQUE (issuer, subject),
    UNIQUE (issuer, organization_id, user_id)
);

CREATE TABLE canonical_identity_audit (
    id uuid PRIMARY KEY,
    user_id text NOT NULL REFERENCES canonical_users(user_id),
    action text NOT NULL
        CHECK (action IN (
            'registered', 'identity_attached', 'email_changed',
            'reconnect_required', 'disabled'
        )),
    actor text NOT NULL CHECK (actor <> ''),
    previous_display_email text,
    new_display_email text,
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE task_submissions
    ADD COLUMN acting_user_id text REFERENCES canonical_users(user_id),
    ADD COLUMN owner_user_id text REFERENCES canonical_users(user_id),
    ADD COLUMN identity_binding_state text NOT NULL DEFAULT 'legacy_reconnect_required'
        CHECK (identity_binding_state IN ('bound', 'legacy_reconnect_required')),
    ADD CONSTRAINT task_submissions_canonical_identity_pair
        CHECK (
            (identity_binding_state = 'bound'
                AND owner_user_id IS NOT NULL
                AND runtime_spec #>> '{canonicalAuthority,schemaVersion}' =
                    'steward/canonical-authority-binding/v1'
                AND runtime_spec #>> '{canonicalAuthority,ownerUserId}' = owner_user_id
                AND ((acting_user IS NULL AND acting_user_id IS NULL)
                    OR (acting_user IS NOT NULL
                        AND acting_user_id = owner_user_id))
                AND ((acting_user_id IS NULL
                        AND runtime_spec #> '{canonicalAuthority,actingUserId}' IS NULL)
                    OR (runtime_spec #>> '{canonicalAuthority,actingUserId}' = acting_user_id)))
            OR
            (identity_binding_state = 'legacy_reconnect_required'
                AND acting_user_id IS NULL
                AND owner_user_id IS NULL
                AND runtime_spec #> '{canonicalAuthority}' IS NULL)
        );

DROP INDEX task_submissions_submitter_idempotency;

CREATE UNIQUE INDEX task_submissions_bound_owner_idempotency
    ON task_submissions (submitter_service, owner_user_id, idempotency_key)
    WHERE identity_binding_state = 'bound';

CREATE UNIQUE INDEX task_submissions_legacy_idempotency
    ON task_submissions (submitter_service, idempotency_key)
    WHERE identity_binding_state = 'legacy_reconnect_required';

COMMENT ON COLUMN task_submissions.identity_binding_state IS
    'Legacy rows are never adopted by email. They require an explicit reconnect/migration decision.';
