-- Keep enforcement observations while distinguishing usage reported by the inference service.
ALTER TABLE spend_observations
    ADD COLUMN usage_reported boolean NOT NULL DEFAULT false,
    ADD COLUMN reported_amount numeric CHECK (reported_amount >= 0),
    ADD COLUMN total_tokens bigint CHECK (total_tokens >= 0),
    ADD CONSTRAINT spend_observations_reported_usage_consistent CHECK (
        (usage_reported AND reported_amount IS NOT NULL)
        OR (NOT usage_reported AND reported_amount IS NULL AND total_tokens IS NULL)
    );
