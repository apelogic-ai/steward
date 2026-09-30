-- Persist only the bounded, adapter-sanitized HTTP status and optional reason
-- produced by the governed provider-control bridge. Raw gateway responses,
-- request URLs, credentials, and arbitrary stderr never belong in this column.
ALTER TABLE connection_operations
    ADD COLUMN failure_detail jsonb,
    ADD CONSTRAINT connection_operations_failure_detail_shape CHECK (
        failure_detail IS NULL
        OR (
            operation_state = 'failed'
            AND failure_category = 'bridge-gateway-http'
            AND jsonb_typeof(failure_detail) = 'object'
            AND failure_detail ? 'upstreamStatus'
            AND failure_detail - 'upstreamStatus' - 'reason' = '{}'::jsonb
            AND jsonb_typeof(failure_detail -> 'upstreamStatus') = 'number'
            AND failure_detail ->> 'upstreamStatus' ~ '^[1-5][0-9]{2}$'
            AND (
                NOT failure_detail ? 'reason'
                OR (
                    jsonb_typeof(failure_detail -> 'reason') = 'string'
                    AND octet_length(failure_detail ->> 'reason') BETWEEN 1 AND 200
                )
            )
        )
    );

COMMENT ON COLUMN connection_operations.failure_detail IS
    'Bounded adapter-sanitized upstream HTTP status and optional reason; never raw provider output, request URLs, or credentials.';
