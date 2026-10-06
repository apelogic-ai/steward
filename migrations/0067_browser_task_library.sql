-- Browser Task drafts are mutable owner-scoped handles. Every saved package is an
-- append-only version whose exact bytes and content digest remain immutable.
CREATE TABLE browser_task_drafts (
    task_id uuid PRIMARY KEY,
    owner_user_id text NOT NULL CHECK (btrim(owner_user_id) <> ''),
    name text NOT NULL
        CHECK (name ~ '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'),
    shared_roles text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT browser_task_drafts_owner_name_key UNIQUE (owner_user_id, name),
    CHECK (cardinality(shared_roles) <= 32)
);

CREATE TABLE browser_task_versions (
    task_id uuid NOT NULL REFERENCES browser_task_drafts(task_id),
    version bigint NOT NULL CHECK (version > 0),
    content_digest text NOT NULL
        CHECK (content_digest ~ '^steward:sha256:[0-9a-f]{64}$'),
    package_path text NOT NULL
        CHECK (octet_length(package_path) BETWEEN 1 AND 512),
    files jsonb NOT NULL CHECK (jsonb_typeof(files) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, version),
    UNIQUE (task_id, content_digest)
);

CREATE INDEX browser_task_drafts_by_owner
ON browser_task_drafts (owner_user_id, updated_at DESC, task_id);

CREATE INDEX browser_task_drafts_by_shared_roles
ON browser_task_drafts USING gin (shared_roles);

CREATE INDEX browser_task_versions_by_digest
ON browser_task_versions (content_digest, created_at DESC, task_id);

CREATE FUNCTION steward_reject_browser_task_draft_identity_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.task_id IS DISTINCT FROM OLD.task_id
        OR NEW.owner_user_id IS DISTINCT FROM OLD.owner_user_id
        OR NEW.name IS DISTINCT FROM OLD.name
        OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'browser Task draft identity is immutable'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER browser_task_draft_identity_is_immutable
BEFORE UPDATE ON browser_task_drafts
FOR EACH ROW EXECUTE FUNCTION steward_reject_browser_task_draft_identity_mutation();

CREATE TRIGGER browser_task_versions_are_immutable
BEFORE UPDATE OR DELETE ON browser_task_versions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

COMMENT ON TABLE browser_task_drafts IS
    'Owner-scoped mutable Task handles; only sharing metadata and the update timestamp may change.';
COMMENT ON TABLE browser_task_versions IS
    'Immutable browser-authored package versions addressed by their exact closure digest.';
