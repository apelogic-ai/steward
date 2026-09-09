-- A delivery lease is scheduling metadata, not an external idempotency fence.
-- Once request invocation is durable, every successor must observe that request.
ALTER TABLE external_effect_outbox ADD COLUMN delivery_invoked_at timestamptz;

-- Conservatively preserve possible effects from the previous dispatcher. It did
-- not distinguish a claimed lease from an external invocation.
UPDATE external_effect_outbox
SET delivery_invoked_at = created_at, generation = generation + 1
WHERE state <> 'delivered' AND attempt_count > 0;

CREATE FUNCTION steward_preserve_approval_delivery_invocation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.delivery_invoked_at IS NOT NULL
        AND NEW.delivery_invoked_at IS DISTINCT FROM OLD.delivery_invoked_at THEN
        RAISE EXCEPTION 'approval delivery invocation is immutable' USING ERRCODE = '55000';
    END IF;
    IF NEW.state = 'failed' AND NEW.delivery_invoked_at IS NOT NULL THEN
        RAISE EXCEPTION 'invoked approval delivery requires external observation before retirement'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER approval_delivery_invocation_is_monotonic
BEFORE UPDATE ON external_effect_outbox
FOR EACH ROW EXECUTE FUNCTION steward_preserve_approval_delivery_invocation();
