CREATE TABLE inference_exhaustions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    runtime_uid text NOT NULL,
    observed_generation bigint NOT NULL,
    spec_digest text NOT NULL CHECK (spec_digest <> ''),
    observed_amount numeric NOT NULL CHECK (observed_amount >= 0),
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX inference_exhaustions_by_runtime
ON inference_exhaustions (
    runtime_uid,
    at DESC,
    id DESC
);

CREATE TRIGGER inference_exhaustions_are_append_only
BEFORE UPDATE OR DELETE ON inference_exhaustions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
