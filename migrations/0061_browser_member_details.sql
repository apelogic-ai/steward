-- Browser member administration may display OIDC profile metadata and the time of the
-- most recent successful browser sign-in. Existing canonical users remain valid with
-- both values absent.
ALTER TABLE canonical_users
    ADD COLUMN display_name text
        CHECK (
            display_name IS NULL
            OR (
                display_name <> ''
                AND display_name = btrim(display_name)
                AND length(display_name) <= 256
            )
        ),
    ADD COLUMN last_sign_in_at timestamptz;

-- Unlinking returns an associated subject to the observed pool. The current subject row
-- changes state, while this append-only ledger preserves who removed the association.
ALTER TABLE federated_subject_audit
    DROP CONSTRAINT federated_subject_audit_action_check,
    ADD CONSTRAINT federated_subject_audit_action_check
        CHECK (action IN (
            'observed', 'v2_seeded', 'connection_verified', 'associated',
            'replaced', 'unassociated', 'disabled'
        ));
