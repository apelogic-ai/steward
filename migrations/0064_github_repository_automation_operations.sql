ALTER TABLE connection_operations
DROP CONSTRAINT connection_operations_operation_kind_check;

ALTER TABLE connection_operations
ADD CONSTRAINT connection_operations_operation_kind_check
CHECK (operation_kind IN (
    'status',
    'start',
    'disconnect',
    'rerun',
    'repositories',
    'workflow',
    'run_status',
    'dispatch',
    'publish'
) AND (
    (operation_kind IN ('status', 'start', 'disconnect') AND authority_version IN (1, 2, 3, 4))
    OR (operation_kind = 'rerun' AND authority_version IN (3, 4))
    OR (operation_kind IN ('repositories', 'workflow', 'run_status', 'dispatch', 'publish')
        AND authority_version = 4)
));

COMMENT ON CONSTRAINT connection_operations_operation_kind_check ON connection_operations IS
    'Allowlisted governed GitHub operations coupled to the first authority version that grants them.';

-- Preserve immutable v1-v3 rows while admitting the exact v4 authority used
-- for repository discovery, workflow reads, dispatch, run status, and PR-only publication.
ALTER TABLE connection_operations
    DROP CONSTRAINT connection_operations_authority_release_check,
    ADD CONSTRAINT connection_operations_authority_release_check CHECK (
        (
            authority_version = 1
            AND authority_digest = 'sha256:7735d22e083daef4bdbd51bb63a652720ef06f5499422e7a8eef4930a6c58663'
            AND mcp_gw_version = '0.3.2'
        )
        OR (
            authority_version = 2
            AND authority_digest = 'sha256:9a572bcefa75b6f2b5b4931d8604c1ad3f3e7560e0e0c2843646ec4f7853ef02'
            AND mcp_gw_version = '0.4.9'
        )
        OR (
            authority_version = 3
            AND authority_digest = 'sha256:d5878c6ae538174c5e0c32ac6aa4617f4ac8e6787b1495b08bd5af9e48f7fbe3'
            AND mcp_gw_version = '0.4.9'
        )
        OR (
            authority_version = 4
            AND authority_digest = 'sha256:6ece401f71b5c71939ef1580b438505380b4f12d29a6fd717ac78a9d9c93848a'
            AND mcp_gw_version = '0.4.9'
        )
    );
