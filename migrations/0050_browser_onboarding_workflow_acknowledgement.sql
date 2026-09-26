-- Keep the explicit "I added the workflow" onboarding acknowledgement with the
-- canonical user's append-only browser preferences so it survives reloads and
-- follows the user across devices.
ALTER TABLE browser_preference_revisions
    ADD COLUMN workflow_acknowledged boolean NOT NULL DEFAULT false;
