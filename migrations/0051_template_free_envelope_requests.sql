-- Envelope requests may either pin an exact catalog revision or carry a complete
-- custom Envelope for explicit administrator review. Keep the template identity
-- as an all-or-nothing pair so no request can imply a revision without its catalog
-- identity (or vice versa).
ALTER TABLE envelope_requests
    ALTER COLUMN template_id DROP NOT NULL,
    ALTER COLUMN template_revision DROP NOT NULL,
    ADD CONSTRAINT envelope_requests_template_reference_pair CHECK (
        (template_id IS NULL) = (template_revision IS NULL)
    );

-- Transition audit mirrors the immutable request fact. Custom requests have no
-- catalog revision to snapshot, while catalog-backed events retain the exact
-- revision that governed the transition.
ALTER TABLE envelope_request_events
    ALTER COLUMN template_revision DROP NOT NULL;

COMMENT ON TABLE envelope_template_revisions IS
    'Sole authoring and provisioning authority for template-backed User Envelopes; legacy role-keyed envelopes are import-only compatibility history.';

COMMENT ON TABLE envelopes IS
    'Compatibility authority retained read-only for one upgrade window; new template authoring and User Envelope provisioning use envelope_template_revisions.';
