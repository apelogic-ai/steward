-- Preserve MCP-GW's bounded machine-readable error code beside the existing
-- upstream status and optional human-readable reason. Existing failure detail
-- rows without a code remain valid and unchanged.
ALTER TABLE connection_operations
    DROP CONSTRAINT connection_operations_failure_detail_shape;

ALTER TABLE connection_operations
    ADD CONSTRAINT connection_operations_failure_detail_shape CHECK (
        failure_detail IS NULL
        OR (
            operation_state = 'failed'
            AND failure_category = 'bridge-gateway-http'
            AND jsonb_typeof(failure_detail) = 'object'
            AND failure_detail ? 'upstreamStatus'
            AND failure_detail - 'upstreamStatus' - 'code' - 'reason' = '{}'::jsonb
            AND jsonb_typeof(failure_detail -> 'upstreamStatus') = 'number'
            AND failure_detail ->> 'upstreamStatus' ~ '^[1-5][0-9]{2}$'
            AND (
                NOT failure_detail ? 'code'
                OR (
                    jsonb_typeof(failure_detail -> 'code') = 'string'
                    AND octet_length(failure_detail ->> 'code') BETWEEN 1 AND 100
                    AND failure_detail ->> 'code' ~ '^[a-z0-9_-]+$'
                )
            )
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
    'Bounded adapter-sanitized upstream HTTP status, optional machine-readable code, and optional reason; never raw provider output, request URLs, or credentials.';
