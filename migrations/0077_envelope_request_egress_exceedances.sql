-- An Envelope request above its template can now exceed it in twelve admission dimensions:
-- egressHosts joins the eleven that migration 0069 bounded. Replace that bound so a valid
-- request above the template in every dimension is stored as pending instead of refused.
DO $$
DECLARE
    exceedance_check text;
BEGIN
    SELECT conname INTO exceedance_check
    FROM pg_constraint
    WHERE conrelid = 'envelope_requests'::regclass
      AND contype = 'c'
      AND pg_get_constraintdef(oid) LIKE '%jsonb_array_length(ceiling_exceedances)%';
    IF exceedance_check IS NULL THEN
        RAISE EXCEPTION 'envelope_requests ceiling_exceedances check constraint not found';
    END IF;
    EXECUTE format('ALTER TABLE envelope_requests DROP CONSTRAINT %I', exceedance_check);
END
$$;

ALTER TABLE envelope_requests
    ADD CONSTRAINT envelope_requests_ceiling_exceedances_check CHECK (
        jsonb_typeof(ceiling_exceedances) = 'array'
        AND jsonb_array_length(ceiling_exceedances) <= 12
        AND (
            template_id IS NOT NULL
            OR jsonb_array_length(ceiling_exceedances) = 0
        )
    );
