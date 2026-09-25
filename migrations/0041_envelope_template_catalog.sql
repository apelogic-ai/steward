-- User-facing Envelope Templates have stable identities independent from the
-- member roles that may select them. Role envelopes remain the authority for
-- legacy runtime admission; this catalog is the authority for User Envelope
-- requests and may expose multiple templates to one role.
CREATE FUNCTION steward_valid_template_member_roles(member_roles text[])
RETURNS boolean
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
AS $$
    SELECT cardinality(member_roles) BETWEEN 1 AND 64
       AND NOT EXISTS (
           SELECT 1
           FROM unnest(member_roles) AS role
           WHERE role !~ '^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$'
       )
       AND member_roles = ARRAY(
           SELECT role
           FROM unnest(member_roles) AS role
           ORDER BY role
       )
       AND cardinality(member_roles) = (
           SELECT count(DISTINCT role)
           FROM unnest(member_roles) AS role
       );
$$;

CREATE TABLE envelope_template_revisions (
    template_id text NOT NULL
        CHECK (template_id ~ '^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$'),
    revision bigint NOT NULL CHECK (revision > 0),
    display_name text NOT NULL
        CHECK (display_name = btrim(display_name)
            AND char_length(display_name) BETWEEN 1 AND 128),
    member_roles text[] NOT NULL
        CHECK (steward_valid_template_member_roles(member_roles)),
    spec jsonb NOT NULL CHECK (jsonb_typeof(spec) = 'object'),
    auto_provision_threshold jsonb
        CHECK (auto_provision_threshold IS NULL
            OR jsonb_typeof(auto_provision_threshold) = 'object'),
    authored_by text NOT NULL CHECK (authored_by <> ''),
    at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (template_id, revision)
);

CREATE INDEX envelope_template_revisions_by_member_roles
    ON envelope_template_revisions USING gin (member_roles);

CREATE TRIGGER envelope_template_revisions_are_append_only
BEFORE UPDATE OR DELETE ON envelope_template_revisions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

-- Preserve every existing role-keyed revision as a same-ID template. The role
-- remains an eligibility assignment rather than the new template identity.
INSERT INTO envelope_template_revisions (
    template_id,
    revision,
    display_name,
    member_roles,
    spec,
    auto_provision_threshold,
    authored_by,
    at
)
SELECT
    scope_ref,
    revision,
    scope_ref,
    ARRAY[scope_ref],
    spec,
    jsonb_build_object('revision', revision, 'spec', spec),
    authored_by,
    at
FROM envelopes
WHERE scope_kind = 'member_role'
ORDER BY scope_ref, revision;

ALTER TABLE envelope_requests
    ADD CONSTRAINT envelope_requests_exact_template_revision
    FOREIGN KEY (template_id, template_revision)
    REFERENCES envelope_template_revisions (template_id, revision);
