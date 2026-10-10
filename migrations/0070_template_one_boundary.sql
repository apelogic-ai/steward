-- A template now has one automatic-provisioning boundary: its published
-- limits. Preserve the stricter boundary of every affected current template
-- by appending a successor whose limits are the old threshold. Historical
-- revisions remain readable exactly as authored.
LOCK TABLE envelope_template_revisions IN SHARE ROW EXCLUSIVE MODE;

WITH latest AS MATERIALIZED (
    SELECT DISTINCT ON (template_id)
        template_id,
        revision,
        display_name,
        member_roles,
        spec,
        auto_provision_threshold,
        allow_inline_browser_tasks
    FROM envelope_template_revisions
    ORDER BY template_id, revision DESC
),
affected AS MATERIALIZED (
    SELECT *
    FROM latest
    WHERE auto_provision_threshold IS NOT NULL
      AND auto_provision_threshold -> 'spec' IS DISTINCT FROM spec
)
INSERT INTO envelope_template_revisions (
    template_id,
    revision,
    display_name,
    member_roles,
    spec,
    auto_provision_threshold,
    allow_inline_browser_tasks,
    authored_by
)
SELECT
    template_id,
    revision + 1,
    display_name,
    member_roles,
    auto_provision_threshold -> 'spec',
    NULL,
    allow_inline_browser_tasks,
    'system:migration'
FROM affected
ORDER BY template_id;

-- Keep the historical column readable while preventing every future writer,
-- including an older process during a rolling deployment, from publishing a
-- new threshold-bearing revision. NOT VALID deliberately preserves old rows.
ALTER TABLE envelope_template_revisions
    ADD CONSTRAINT envelope_template_revisions_retired_auto_provision_threshold
    CHECK (auto_provision_threshold IS NULL) NOT VALID;
