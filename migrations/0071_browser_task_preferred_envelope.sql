-- A browser-authored Task version remembers the Envelope selected while it was
-- authored so a later Run starts from the same reviewed authority choice.
ALTER TABLE browser_task_versions
ADD COLUMN preferred_envelope_digest text
    CHECK (
        preferred_envelope_digest IS NULL
        OR preferred_envelope_digest ~ '^steward:sha256:[0-9a-f]{64}$'
    );

COMMENT ON COLUMN browser_task_versions.preferred_envelope_digest IS
    'Optional immutable Envelope preference selected while authoring this Task version; Run admission still validates current authority.';
