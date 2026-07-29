CREATE TABLE spend_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    runtime_uid text NOT NULL,
    observed_amount numeric NOT NULL CHECK (observed_amount >= 0),
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    exhausted boolean NOT NULL,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX spend_observations_by_runtime
ON spend_observations (runtime_uid, at DESC);

CREATE TRIGGER spend_observations_are_append_only
BEFORE UPDATE OR DELETE ON spend_observations
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
