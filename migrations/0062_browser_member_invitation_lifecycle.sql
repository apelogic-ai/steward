-- Preserve revoked invitations as immutable membership history while allowing a later
-- invitation for the same verified organization email to create a fresh pending member.
ALTER TABLE canonical_users
    DROP CONSTRAINT canonical_users_state_check,
    ADD CONSTRAINT canonical_users_state_check
        CHECK (state IN ('active', 'pending', 'reconnect_required', 'disabled', 'revoked'));

ALTER TABLE canonical_identity_audit
    DROP CONSTRAINT canonical_identity_audit_action_check,
    ADD CONSTRAINT canonical_identity_audit_action_check
        CHECK (action IN (
            'registered', 'preprovisioned', 'activated', 'identity_attached',
            'email_changed', 'reconnect_required', 'disabled', 'enabled',
            'invitation_revoked'
        ));

DROP INDEX canonical_users_one_display_email_per_organization;
CREATE UNIQUE INDEX canonical_users_one_live_display_email_per_organization
    ON canonical_users (organization_id, lower(display_email))
    WHERE state <> 'revoked';
