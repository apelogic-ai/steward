ALTER TABLE approvals
RENAME COLUMN jira_key TO decision_key;

ALTER TABLE admission_decisions
ADD COLUMN base_spec_digest text;

ALTER TABLE admission_decisions
ADD CONSTRAINT admission_decisions_require_base_spec_digest
CHECK (base_spec_digest IS NOT NULL AND base_spec_digest <> '') NOT VALID;

CREATE UNIQUE INDEX admission_decisions_exact_request
ON admission_decisions (
    runtime_uid,
    spec_digest,
    envelope_rev,
    base_spec_digest,
    actor,
    member_role
);

ALTER TABLE grants
ADD COLUMN envelope_revision bigint;

ALTER TABLE grants
ADD CONSTRAINT grants_require_envelope_revision
CHECK (envelope_revision IS NOT NULL AND envelope_revision > 0) NOT VALID;

ALTER TABLE grants
ADD CONSTRAINT grants_require_expiry
CHECK (expires_at IS NOT NULL) NOT VALID;

CREATE TABLE grant_revocations (
    grant_id uuid PRIMARY KEY REFERENCES grants(id),
    revoked_by text NOT NULL,
    reason text NOT NULL CHECK (reason <> ''),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER grant_revocations_are_append_only
BEFORE UPDATE OR DELETE ON grant_revocations
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
