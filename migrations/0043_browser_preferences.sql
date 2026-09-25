-- Browser preferences follow the canonical user across devices. Revisions are
-- append-only so dismissal and theme changes remain attributable without
-- turning identity-provider data into application authority.
CREATE TABLE browser_preference_revisions (
    user_id text NOT NULL REFERENCES canonical_users(user_id),
    revision bigint NOT NULL CHECK (revision > 0),
    onboarding_dismissed boolean NOT NULL,
    theme text CHECK (theme IS NULL OR theme IN ('light', 'dark', 'system')),
    actor text NOT NULL CHECK (actor <> ''),
    at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, revision)
);

CREATE TRIGGER browser_preference_revisions_are_append_only
BEFORE UPDATE OR DELETE ON browser_preference_revisions
FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation();
