ALTER TABLE approvals
ADD COLUMN decision_filing_token uuid;

ALTER TABLE approvals
ADD COLUMN decision_filing_started_at timestamptz;

ALTER TABLE approvals
ADD CONSTRAINT approvals_filing_claim_is_complete
CHECK (
    (decision_filing_token IS NULL AND decision_filing_started_at IS NULL)
    OR
    (decision_filing_token IS NOT NULL AND decision_filing_started_at IS NOT NULL)
);
