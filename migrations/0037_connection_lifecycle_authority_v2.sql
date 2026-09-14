-- Preserve existing v1 rows while admitting only the immutable v2 lifecycle contract.
ALTER TABLE connection_operations
    DROP CONSTRAINT connection_operations_authority_version_check,
    DROP CONSTRAINT connection_operations_authority_digest_check,
    DROP CONSTRAINT connection_operations_mcp_gw_version_check,
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
    );
