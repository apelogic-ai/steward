CREATE TABLE connection_operations (
    operation_id uuid PRIMARY KEY,
    task_uid uuid NOT NULL UNIQUE REFERENCES task_submissions(task_uid),
    canonical_user_id text NOT NULL REFERENCES canonical_users(user_id),
    provider text NOT NULL CHECK (provider = 'github'),
    operation_kind text NOT NULL
        CHECK (operation_kind IN ('status', 'start', 'disconnect')),
    submitter_service text NOT NULL CHECK (submitter_service = 'steward-connections'),
    authority_id text NOT NULL CHECK (authority_id = 'steward-connections'),
    authority_version bigint NOT NULL CHECK (authority_version = 1),
    authority_digest text NOT NULL
        CHECK (authority_digest = 'sha256:7735d22e083daef4bdbd51bb63a652720ef06f5499422e7a8eef4930a6c58663'),
    runtime_spec_snapshot jsonb NOT NULL
        CHECK (jsonb_typeof(runtime_spec_snapshot) = 'object'),
    command_snapshot jsonb NOT NULL
        CHECK (jsonb_typeof(command_snapshot) = 'array'),
    bridge_image_digest text NOT NULL
        CHECK (bridge_image_digest ~ '^.+@sha256:[0-9a-f]{64}$'),
    mcp_gw_origin text NOT NULL CHECK (btrim(mcp_gw_origin) <> ''),
    mcp_gw_version text NOT NULL CHECK (mcp_gw_version = '0.3.2'),
    runtime_namespace text NOT NULL CHECK (btrim(runtime_namespace) <> ''),
    runtime_class text NOT NULL CHECK (btrim(runtime_class) <> ''),
    idempotency_identity text NOT NULL CHECK (btrim(idempotency_identity) <> ''),
    uncached_status boolean NOT NULL DEFAULT false,
    operation_state text NOT NULL DEFAULT 'queued'
        CHECK (operation_state IN ('queued', 'provisioning', 'running', 'succeeded', 'failed')),
    oauth_phase text NOT NULL DEFAULT 'none'
        CHECK (oauth_phase IN ('none', 'pending', 'completed', 'expired')),
    authorization_url text,
    authorization_url_digest text
        CHECK (authorization_url_digest IS NULL
            OR authorization_url_digest ~ '^sha256:[0-9a-f]{64}$'),
    flow_created_at timestamptz,
    flow_expires_at timestamptz,
    cached_status jsonb,
    cache_expires_at timestamptz,
    result jsonb,
    result_expires_at timestamptz,
    failure_category text CHECK (failure_category IS NULL OR btrim(failure_category) <> ''),
    finalization_state text NOT NULL DEFAULT 'not_requested'
        CHECK (finalization_state IN ('not_requested', 'requested', 'finalized')),
    cleanup_state text NOT NULL DEFAULT 'pending'
        CHECK (cleanup_state IN ('pending', 'tearing_down', 'clean', 'stalled')),
    cleanup_finding text CHECK (cleanup_finding IS NULL OR btrim(cleanup_finding) <> ''),
    response_deadline_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (canonical_user_id, provider, idempotency_identity),
    CHECK (
        (oauth_phase = 'none'
            AND authorization_url IS NULL
            AND flow_created_at IS NULL
            AND flow_expires_at IS NULL)
        OR
        (operation_kind = 'start'
            AND oauth_phase IN ('pending', 'completed', 'expired')
            AND flow_created_at IS NOT NULL
            AND flow_expires_at IS NOT NULL)
    ),
    CHECK (authorization_url IS NULL OR oauth_phase = 'pending'),
    CHECK (cached_status IS NULL OR operation_kind = 'status'),
    CHECK (NOT uncached_status OR operation_kind = 'status'),
    CHECK (NOT (finalization_state = 'finalized' AND cleanup_state <> 'clean'))
);

CREATE INDEX connection_operations_by_owner_provider
ON connection_operations (canonical_user_id, provider, created_at DESC);

CREATE UNIQUE INDEX connection_operations_one_active_runtime
ON connection_operations (canonical_user_id, provider)
WHERE operation_state IN ('queued', 'provisioning', 'running')
  AND finalization_state = 'not_requested';

CREATE UNIQUE INDEX connection_operations_one_pending_oauth_flow
ON connection_operations (canonical_user_id, provider)
WHERE operation_kind = 'start' AND oauth_phase = 'pending';

COMMENT ON TABLE connection_operations IS
    'Dedicated governed provider-control projection; rows are excluded from generic Task and agent-run read models.';

COMMENT ON COLUMN connection_operations.authorization_url IS
    'Sensitive transient OAuth continuation returned only by the authenticated Connections BFF and redacted after completion or expiry.';
