-- Record GitHub Connect as a distinct, auditable proof for federated-subject
-- association. Existing bindings and audit rows remain unchanged; the new proof
-- column is backfilled from the current association's audit revision.
ALTER TABLE federated_subjects
    ADD COLUMN association_method text;

UPDATE federated_subjects AS subject
SET association_method = CASE
    WHEN EXISTS (
        SELECT 1
        FROM federated_subject_audit AS audit
        WHERE audit.subject_id = subject.subject_id
          AND audit.revision = subject.revision
          AND audit.action = 'v2_seeded'
    ) THEN 'v2-claim'
    ELSE 'admin'
END
WHERE subject.state = 'associated';

ALTER TABLE federated_subjects
    ADD CONSTRAINT federated_subjects_association_method CHECK (
        association_method IS NULL
        OR association_method IN ('admin', 'connection-verification', 'v2-claim')
    ),
    ADD CONSTRAINT federated_subjects_association_method_state CHECK (
        state <> 'associated' OR association_method IS NOT NULL
    );

ALTER TABLE federated_subject_audit
    DROP CONSTRAINT federated_subject_audit_action_check;

ALTER TABLE federated_subject_audit
    ADD CONSTRAINT federated_subject_audit_action_check
    CHECK (
        action IN (
            'observed',
            'v2_seeded',
            'connection_verified',
            'associated',
            'replaced',
            'disabled'
        )
    );

ALTER TABLE federated_subject_audit
    ADD COLUMN connection_provider text,
    ADD COLUMN connection_account_id text,
    ADD CONSTRAINT federated_subject_audit_connection_evidence CHECK (
        (
            action = 'connection_verified'
            AND connection_provider = 'github'
            AND connection_account_id <> ''
            AND length(connection_account_id) <= 20
            AND connection_account_id ~ '^[1-9][0-9]*$'
        )
        OR (
            action <> 'connection_verified'
            AND connection_provider IS NULL
            AND connection_account_id IS NULL
        )
    );

COMMENT ON COLUMN federated_subject_audit.connection_provider IS
    'Provider whose authenticated connection proved the immutable external account identity; populated only for connection_verified.';

COMMENT ON COLUMN federated_subject_audit.connection_account_id IS
    'Immutable provider account identifier used for connection verification; never a login, email, or display name.';

COMMENT ON COLUMN federated_subjects.association_method IS
    'Proof method for the current association: administrator action, verified connection, or legacy v2 canonical claim.';
