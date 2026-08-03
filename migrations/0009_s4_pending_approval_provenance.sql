ALTER TABLE admission_decisions
ADD COLUMN base_pending_approval_digest text;

ALTER TABLE admission_decisions
ADD CONSTRAINT admission_decisions_nonempty_base_pending_approval_digest
CHECK (
    base_pending_approval_digest IS NULL
    OR base_pending_approval_digest <> ''
);
