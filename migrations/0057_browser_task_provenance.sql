-- Browser-authored Tasks share the governed Task lifecycle but carry provenance that is
-- intentionally distinct from GitHub-signed source authority. Persist the resolved immutable
-- locator (and inline bytes, when applicable) without weakening the existing direct evidence.
ALTER TABLE task_submissions
    ADD COLUMN task_origin text NOT NULL DEFAULT 'unknown',
    ADD COLUMN browser_task_evidence jsonb;

ALTER TABLE envelope_template_revisions
    ADD COLUMN allow_inline_browser_tasks boolean NOT NULL DEFAULT true;

UPDATE task_submissions
SET task_origin = CASE
    WHEN EXISTS (
        SELECT 1 FROM connection_operations operations
        WHERE operations.task_uid = task_submissions.task_uid
    ) THEN 'connections'
    WHEN source_provenance IS NOT NULL THEN 'github-actions'
    ELSE 'unknown'
END
WHERE orchestration_version = 3;

ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_task_origin_valid CHECK (
        task_origin IN ('browser', 'github-actions', 'connections', 'unknown')
    ),
    ADD CONSTRAINT task_submissions_browser_evidence_origin CHECK (
        (task_origin = 'browser') = (browser_task_evidence IS NOT NULL)
    ),
    ADD CONSTRAINT task_submissions_browser_evidence_shape CHECK (
        browser_task_evidence IS NULL
        OR (
            jsonb_typeof(browser_task_evidence) = 'object'
            AND octet_length(browser_task_evidence ->> 'source') BETWEEN 1 AND 512
            AND (
                browser_task_evidence ->> 'revision' ~ '^git:sha1:[0-9a-f]{40}$'
                OR browser_task_evidence ->> 'revision' ~ '^steward:sha256:[0-9a-f]{64}$'
                OR browser_task_evidence ->> 'revision' ~ '^steward:version:[1-9][0-9]*$'
            )
            AND octet_length(browser_task_evidence ->> 'path') BETWEEN 1 AND 512
            AND browser_task_evidence ->> 'closureDigest' ~ '^steward:sha256:[0-9a-f]{64}$'
            AND (
                jsonb_typeof(browser_task_evidence -> 'closure') = 'object'
                OR browser_task_evidence ->> 'source' LIKE 'steward:registry/%'
            )
            AND (
                NOT (browser_task_evidence ? 'inlineFiles')
                OR jsonb_typeof(browser_task_evidence -> 'inlineFiles') = 'object'
            )
        ) IS TRUE
    );

CREATE FUNCTION steward_reject_browser_task_provenance_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.task_origin IS DISTINCT FROM OLD.task_origin
        OR NEW.browser_task_evidence IS DISTINCT FROM OLD.browser_task_evidence THEN
        RAISE EXCEPTION 'browser Task provenance is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_browser_provenance_is_immutable
BEFORE UPDATE OF task_origin, browser_task_evidence ON task_submissions
FOR EACH ROW EXECUTE FUNCTION steward_reject_browser_task_provenance_mutation();

COMMENT ON COLUMN task_submissions.task_origin IS
    'Immutable authoring origin: browser, github-actions, connections, or unknown historical origin.';
COMMENT ON COLUMN task_submissions.browser_task_evidence IS
    'Immutable browser-authored package locator, resolved pin, closure, and inline bytes when present.';
