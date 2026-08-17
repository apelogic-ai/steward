-- Google OIDC establishes a canonical person. This append-only ledger records only
-- Steward-local authorization decisions for that opaque user ID; provider claims and
-- email are intentionally absent from the authority boundary.
CREATE TABLE browser_rbac_assignment_events (
    id uuid PRIMARY KEY,
    user_id text NOT NULL REFERENCES canonical_users(user_id),
    assignment_kind text NOT NULL
        CHECK (assignment_kind IN ('administrator', 'member_role')),
    member_role text,
    action text NOT NULL CHECK (action IN ('grant', 'revoke')),
    actor text NOT NULL CHECK (actor <> ''),
    at timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (assignment_kind = 'administrator' AND member_role IS NULL)
        OR
        (assignment_kind = 'member_role'
            AND member_role ~ '^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$')
    )
);

CREATE INDEX browser_rbac_assignment_events_current_by_user
    ON browser_rbac_assignment_events
        (user_id, assignment_kind, member_role, at DESC, id DESC);

CREATE TRIGGER browser_rbac_assignment_events_are_append_only
BEFORE UPDATE OR DELETE ON browser_rbac_assignment_events
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
