ALTER TABLE envelopes
    DROP CONSTRAINT envelopes_scope_kind_check;

ALTER TABLE envelopes
    ADD CONSTRAINT envelopes_scope_kind_check
    CHECK (scope_kind IN ('member_role', 'service'));
