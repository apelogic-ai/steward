-- Envelope-request decisions carry the same bounded administrator rationale,
-- evidence, and optional expiry metadata as runtime-exception decisions.
ALTER TABLE envelope_request_events
    ADD COLUMN rationale text,
    ADD COLUMN evidence_url text,
    ADD COLUMN expires_at timestamptz,
    ADD CONSTRAINT envelope_request_events_rationale_nonempty
        CHECK (rationale IS NULL OR rationale <> ''),
    ADD CONSTRAINT envelope_request_events_evidence_url_nonempty
        CHECK (evidence_url IS NULL OR evidence_url <> '');

-- Filing an external decision reference is a separate append-only fact. The
-- unique request binding makes an identical retry safe and conflicting output
-- fail closed without rewriting history.
CREATE TABLE envelope_request_decision_references (
    request_id uuid PRIMARY KEY REFERENCES envelope_requests(id),
    decision_key text NOT NULL CHECK (decision_key <> ''),
    evidence_url text NOT NULL CHECK (evidence_url <> ''),
    filed_by text NOT NULL CHECK (filed_by <> ''),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER envelope_request_decision_references_are_append_only
BEFORE UPDATE OR DELETE ON envelope_request_decision_references
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
