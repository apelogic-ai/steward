-- New revisions may select one execution model without narrowing the User
-- Envelope. Historical revisions remain immutable and retain their original
-- single-model Envelope compatibility path.
ALTER TABLE workflow_revisions
    ADD COLUMN model_provider text,
    ADD COLUMN model_name text,
    ADD CONSTRAINT workflow_revisions_model_pair CHECK (
        (model_provider IS NULL AND model_name IS NULL)
        OR (
            model_provider IS NOT NULL AND btrim(model_provider) <> ''
            AND model_provider = btrim(model_provider)
            AND model_name IS NOT NULL AND btrim(model_name) <> ''
            AND model_name = btrim(model_name)
        )
    );
