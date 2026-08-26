-- Workflows are immutable, administrator-published execution definitions. A
-- changed definition is a new positive version; existing versions remain exact
-- provenance for every Task that used them.
CREATE TABLE workflow_revisions (
    name text NOT NULL
        CHECK (name ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'),
    version bigint NOT NULL CHECK (version > 0),
    display_name text NOT NULL CHECK (btrim(display_name) <> ''),
    agent text NOT NULL
        CHECK (agent ~ '^[^@[:space:]]+@[^@[:space:]]+$'),
    prompt text NOT NULL CHECK (btrim(prompt) <> ''),
    content_digest text NOT NULL
        CHECK (content_digest ~ '^sha256:[0-9a-f]{64}$'),
    published_by text NOT NULL CHECK (btrim(published_by) <> ''),
    published_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (name, version),
    UNIQUE (name, version, content_digest)
);

CREATE TRIGGER workflow_revisions_are_immutable
BEFORE UPDATE OR DELETE ON workflow_revisions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

ALTER TABLE task_submissions
    ADD COLUMN workflow_name text,
    ADD COLUMN workflow_version bigint,
    ADD COLUMN workflow_digest text,
    ADD COLUMN user_envelope_instance_id text,
    ADD COLUMN user_envelope_revision bigint,
    ADD COLUMN user_envelope_digest text,
    ADD CONSTRAINT task_submissions_versioned_pins_complete
        CHECK (
            (workflow_name IS NULL
                AND workflow_version IS NULL
                AND workflow_digest IS NULL
                AND user_envelope_instance_id IS NULL
                AND user_envelope_revision IS NULL
                AND user_envelope_digest IS NULL)
            OR
            (workflow_name ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'
                AND workflow_version > 0
                AND workflow_digest ~ '^sha256:[0-9a-f]{64}$'
                AND btrim(user_envelope_instance_id) <> ''
                AND user_envelope_revision > 0
                AND user_envelope_digest ~ '^sha256:[0-9a-f]{64}$')
        ),
    ADD CONSTRAINT task_submissions_workflow_revision_fk
        FOREIGN KEY (workflow_name, workflow_version, workflow_digest)
        REFERENCES workflow_revisions (name, version, content_digest);

COMMENT ON TABLE workflow_revisions IS
    'Immutable server-published Workflow revisions; changes append the next version.';

COMMENT ON COLUMN task_submissions.workflow_digest IS
    'Exact immutable Workflow content digest pinned when a versioned Task is admitted.';

COMMENT ON COLUMN task_submissions.user_envelope_digest IS
    'Exact provisioned User Envelope digest pinned when a versioned Task is admitted.';
