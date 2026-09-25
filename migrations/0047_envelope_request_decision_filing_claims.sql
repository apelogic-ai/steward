-- External decision creation is an ambiguous side effect. A short-lived claim serializes filing
-- per Envelope request so concurrent browser retries cannot create multiple Jira decisions.
-- The claim is operational state; the completed reference remains append-only audit evidence.
CREATE TABLE envelope_request_decision_filing_claims (
    request_id uuid PRIMARY KEY REFERENCES envelope_requests(id),
    token uuid NOT NULL,
    claimed_by text NOT NULL CHECK (claimed_by <> ''),
    started_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX envelope_request_decision_filing_claims_by_started_at
    ON envelope_request_decision_filing_claims (started_at);
