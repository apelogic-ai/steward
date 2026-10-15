CREATE TABLE inference_spend_episodes (
    runtime_uid text NOT NULL,
    inference_reference text NOT NULL CHECK (btrim(inference_reference) <> ''),
    baseline_amount numeric NOT NULL CHECK (baseline_amount >= 0),
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (runtime_uid, inference_reference)
);

CREATE TRIGGER inference_spend_episodes_are_append_only
BEFORE UPDATE OR DELETE ON inference_spend_episodes
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();

COMMENT ON TABLE inference_spend_episodes IS
    'Immutable cumulative spend baseline bound to one runtime gateway-key reference.';
