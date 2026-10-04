-- An administrator may reserve a Steward member for one verified organization
-- email before that person first signs in. A pending record is not a principal:
-- registration activates it only after the OIDC boundary verifies the same email.
ALTER TABLE canonical_users
    DROP CONSTRAINT canonical_users_state_check,
    ADD CONSTRAINT canonical_users_state_check
        CHECK (state IN ('active', 'pending', 'reconnect_required', 'disabled'));

ALTER TABLE canonical_identity_audit
    DROP CONSTRAINT canonical_identity_audit_action_check,
    ADD CONSTRAINT canonical_identity_audit_action_check
        CHECK (action IN (
            'registered', 'preprovisioned', 'activated', 'identity_attached',
            'email_changed', 'reconnect_required', 'disabled'
        ));
