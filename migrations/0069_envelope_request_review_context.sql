-- Keep the requester-provided review context and the exact template comparison
-- beside the immutable Envelope request fact. Historical requests predate this
-- contract and therefore carry neither a justification nor recorded deltas.
ALTER TABLE envelope_requests
    ADD COLUMN justification text
        CHECK (
            justification IS NULL
            OR (
                justification = btrim(justification)
                AND char_length(justification) BETWEEN 1 AND 2000
            )
        ),
    ADD COLUMN ceiling_exceedances jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (
            jsonb_typeof(ceiling_exceedances) = 'array'
            AND jsonb_array_length(ceiling_exceedances) <= 11
            AND (
                template_id IS NOT NULL
                OR jsonb_array_length(ceiling_exceedances) = 0
            )
        );

COMMENT ON COLUMN envelope_requests.justification IS
    'Optional immutable requester explanation for administrator review.';
COMMENT ON COLUMN envelope_requests.ceiling_exceedances IS
    'Exact admission deltas against the pinned template revision at request creation.';
