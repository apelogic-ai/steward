ALTER TABLE connection_operations
ADD COLUMN artifact_trust_mode text DEFAULT 'github-attestation';

UPDATE connection_operations
SET artifact_trust_mode = 'github-attestation'
WHERE artifact_trust_mode IS NULL;

ALTER TABLE connection_operations
ALTER COLUMN artifact_trust_mode SET NOT NULL;

ALTER TABLE connection_operations
ADD CONSTRAINT connection_operations_artifact_trust_mode_is_known
CHECK (artifact_trust_mode IN ('github-attestation', 'operator-pinned'));

COMMENT ON COLUMN connection_operations.artifact_trust_mode IS
    'Server-authored artifact trust policy pinned with the immutable provider-control execution binding; the default preserves old writers during rolling upgrades.';
