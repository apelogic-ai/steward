# Migrations

SQL migrations are append-only. Slice S3 introduces Postgres operational state
for immutable envelope revisions, admission decisions, the approval queue, and
runtime-event history. Slice S4 adds immutable, runtime-UID-bound grants linked
to the Steward approval that authorized each exception. Migration 0011 adds
durable Task submission, execution, archive, and finalization state for the
single-shot Task API.

Migration 0012 introduces opaque canonical person identities. Existing Task
rows remain explicitly `legacy_reconnect_required`; the migration never derives
a person ID from email, issuer, or another mutable claim.

`cargo xtask migrate-check` rejects edits or renames of migrations already
present on the comparison base. The S3 and S4 store integration tests apply the
full set to empty ephemeral Postgres databases.
