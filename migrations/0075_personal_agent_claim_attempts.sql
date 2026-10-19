-- Bound repeated dispatcher reclaims independently from Task-start retries.
ALTER TABLE personal_agent_turns
    ADD COLUMN claim_attempts integer NOT NULL DEFAULT 0 CHECK (claim_attempts >= 0);
